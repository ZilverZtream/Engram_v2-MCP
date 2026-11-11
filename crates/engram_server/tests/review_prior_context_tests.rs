//! Wanted behavior: review consults prior evidence automatically, discloses its
//! limits, and retains findings against changed code despite earlier decisions.
use engram_core::{Config, ContentHash, RelPath};
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};
use std::path::Path;

// Deliberately invented text, never an actual credential.
const AFTER: &str = "pub fn export() { let password = \"fixture_dummy_value_only\"; }\n";
const DIFF: &str = "diff --git a/src/export.rs b/src/export.rs\n--- a/src/export.rs\n+++ b/src/export.rs\n@@ -1 +1 @@\n-pub fn export() {}\n+pub fn export() { let password = \"fixture_dummy_value_only\"; }\n";

fn text(response: &rmcp::model::CallToolResult) -> &str {
    &response.content[0].as_text().unwrap().text
}

async fn fixture(parent: &Path) -> anyhow::Result<(Engram, String, std::path::PathBuf)> {
    let root = parent.join("repo");
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(root.join("src/export.rs"), "pub fn export() {}\n")?;
    {
        let repo = git2::Repository::init(&root)?;
        let mut index = repo.index()?;
        index.add_path(Path::new("src/export.rs"))?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
        repo.commit(Some("HEAD"), &signature, &signature, "baseline", &tree, &[])?;
    }
    std::fs::write(root.join("src/export.rs"), AFTER)?;
    let data_dir = parent.join("data");
    std::fs::create_dir(&data_dir)?;
    let (state, _) = AppState::new(Config {
        data_dir,
        allowed_roots: vec![root.clone()],
        embedding_backend: if cfg!(feature = "vector") {
            "local"
        } else {
            "fts_only"
        }
        .into(),
        ..Default::default()
    })?;
    let engram = Engram::new(state);
    let indexed = engram.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"Prior review behavior","project_type":"rust","wait":true
    }))?).await?;
    let pid = text(&indexed)
        .split("project_id: ")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("missing project ID"))?
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned();
    Ok((engram, pid, root))
}

fn thread(pr: u64, file: &str, long: bool) -> Value {
    let detail = if long {
        "Additional historical finding detail. ".repeat(60)
    } else {
        String::new()
    };
    let rationale = if long {
        "Historical discussion needing full source recovery. ".repeat(50)
    } else {
        "This is the intended behaviour for the old demonstration fixture only; production safeguards remain required.".into()
    };
    json!({"pr_id":pr,"pr_title":"Export password demonstration","pr_author":"Fixture author",
        "pr_date":"2026-01-01T12:00:00Z","thread_id":1,"status":"fixed","file_path":file,"line":1,
        "comments":[{"author":"Review bot","text":format!("The export password uses a hardcoded fixture value. {detail}")},
            {"author":"Owner","text":rationale}]})
}

async fn ingest(engram: &Engram, pid: &str, root: &Path, records: &[Value]) -> anyhow::Result<()> {
    std::fs::write(
        root.join("review-fixture.jsonl"),
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )?;
    engram
        .handle_ingest_review_verdicts(serde_json::from_value(json!({"project_id":pid,
        "decision_makers":["Owner"],"source":"json_file","file_path":"review-fixture.jsonl"}))?)
        .await?;
    Ok(())
}

async fn review(
    engram: &Engram,
    pid: &str,
    json_output: bool,
) -> anyhow::Result<rmcp::model::CallToolResult> {
    Ok(engram
        .handle_pre_commit_review(serde_json::from_value(json!({
            "project_id":pid,"diff":DIFF,"output_json":json_output,"max_findings":100
        }))?)
        .await?)
}

fn corpus_doc(namespace: &str, number: u64) -> engram_index::IndexDoc {
    engram_index::IndexDoc {
        generation: 1,
        chunk_id: number,
        path: RelPath::new(&format!("src/history-{number}.rs")),
        language: "rust".into(),
        content: AFTER.into(),
        namespace: namespace.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: format!("{namespace}-{number}"),
        content_hash: ContentHash::compute(AFTER.as_bytes()).0,
    }
}

#[tokio::test]
async fn default_review_supplies_prior_ruling_provenance_in_both_formats_without_weakening_findings()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let (engram, pid, root) = fixture(temporary.path()).await?;
        let search = engram.state.get_project_cached(&pid).unwrap().search.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        search.index_docs(&pid, &[corpus_doc("antipattern",1),corpus_doc("antipattern",2)], &cancel).await?;
        let baseline: Value = serde_json::from_str(text(&review(&engram,&pid,true).await?))?;
        let before = baseline["findings"].as_array().unwrap().iter().find(|f| f["gate"]=="antipattern")
            .ok_or_else(|| anyhow::anyhow!("fixture needs actual anti-pattern finding: {baseline}"))?;
        let before_severity = before["severity"].clone();
        anyhow::ensure!(baseline["prior_review_context"]["status"]=="no_indexed_matches");
        anyhow::ensure!(baseline["prior_review_context"]["coverage"].as_str().unwrap().contains("do not prove absence"));
        ingest(&engram,&pid,&root,&[thread(100,"/src/export.rs",false)]).await?;
        engram.handle_record_review_decisions(serde_json::from_value(json!({"project_id":pid,"review_id":"PR-100",
            "decisions":[{"event_id":"explicit-ruling","finding_id":"thread:1","supersedes":null,
                "kind":"accepted_exception","source_url":"https://example.invalid/review/100/thread/1",
                "author":"Owner","recorded_at":"2026-01-01T13:00:00Z",
                "rationale":"Old fixture-only exception; preserve production guards and recheck changed code.","verification":null}]}))?).await?;
        search.index_docs(&pid,&[corpus_doc("wontfix_patterns",3)],&cancel).await?;
        let response: Value = serde_json::from_str(text(&review(&engram,&pid,true).await?))?;
        let finding = response["findings"].as_array().unwrap().iter().find(|f| f["gate"]=="antipattern").unwrap();
        anyhow::ensure!(finding["severity"]==before_severity, "Workflow-only pattern must not dampen current severity");
        anyhow::ensure!(response["findings"].as_array().unwrap().iter().any(|f|f["gate"]=="secret_leakage"));
        let context = &response["prior_review_context"];
        anyhow::ensure!(context["automatic_finding_suppression"]==false);
        let item = &context["items"][0];
        anyhow::ensure!(item["retrieval_relation"]=="exact_file_reference");
        anyhow::ensure!(item["stored_text"].as_str().unwrap().contains("intended behaviour"));
        anyhow::ensure!(item["indexed_source"]["content_blake3"].as_str().unwrap().len()==64);
        anyhow::ensure!(item["provider_neutral_events"][0]["evidence"]["source_url"]=="https://example.invalid/review/100/thread/1");
        anyhow::ensure!(item["provider_neutral_events"][0]["current_worktree_and_exception_applicability"]=="not_established");
        anyhow::ensure!(item["original_source_url"].is_null());
        anyhow::ensure!(item["original_discussion_coverage"].as_str().unwrap().contains("not_complete"));
        let markdown = review(&engram,&pid,false).await?;
        anyhow::ensure!(text(&markdown).contains("Prior review context") && text(&markdown).contains("Old fixture-only exception"));
        anyhow::ensure!(text(&markdown).contains("Hardcoded secret detected"));
        let key = format!("review_decisions:{}", blake3::hash(b"PR-100").to_hex());
        engram.state.registry.set_meta(&pid,&key,"unreadable imported history")?;
        let unavailable: Value = serde_json::from_str(text(&review(&engram,&pid,true).await?))?;
        anyhow::ensure!(unavailable["prior_review_context"]["items"][0]["provider_neutral_status"]["status"]=="unavailable");
        anyhow::ensure!(unavailable["findings"].as_array().unwrap().iter().any(|f|f["gate"]=="secret_leakage"));
        anyhow::ensure!(unavailable["findings"].as_array().unwrap().iter().find(|f|f["gate"]=="antipattern").unwrap()["severity"]==before_severity);
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn review_context_does_not_promote_basename_relevance_or_attested_commit_into_current_applicability()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let (engram,pid,root) = fixture(temporary.path()).await?;
        ingest(&engram,&pid,&root,&[thread(101,"/other/export.rs",false)]).await?;
        let head = git2::Repository::open(&root)?.head()?.target().unwrap().to_string();
        engram.handle_record_review_decisions(serde_json::from_value(json!({"project_id":pid,"review_id":"PR-101",
            "decisions":[{"event_id":"external-check","finding_id":"different-finding","supersedes":null,
                "kind":"verified_fix","source_url":"https://example.invalid/review/101","author":"External verifier",
                "recorded_at":"2026-01-01T13:00:00Z","rationale":"External declaration for different reviewed code.",
                "verification":{"commit":head,"check":"retained external check","evidence_url":"https://example.invalid/check",
                    "artifact_sha256":"a".repeat(64),"verifier":"External verifier"}}]}))?).await?;
        let response: Value = serde_json::from_str(text(&review(&engram,&pid,true).await?))?;
        let item = &response["prior_review_context"]["items"][0];
        anyhow::ensure!(item["retrieval_relation"]=="basename_only");
        let event = &item["provider_neutral_events"][0];
        anyhow::ensure!(event["association"]=="same_review_only");
        anyhow::ensure!(event["verification_target_matches_observed_head"]==true);
        anyhow::ensure!(event["effective_status"]=="verification_not_established_for_current_worktree");
        anyhow::ensure!(event["provider_identity_authenticated"]==false);
        anyhow::ensure!(response["findings"].as_array().unwrap().iter().any(|f|f["gate"]=="secret_leakage"));
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn review_context_is_bounded_and_names_omissions_instead_of_claiming_no_rulings()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let (engram, pid, root) = fixture(temporary.path()).await?;
        let records = (1..=30)
            .map(|n| thread(n, "/src/export.rs", true))
            .collect::<Vec<_>>();
        ingest(&engram, &pid, &root, &records).await?;
        let response: Value = serde_json::from_str(text(&review(&engram, &pid, true).await?))?;
        let context = &response["prior_review_context"];
        anyhow::ensure!(serde_json::to_vec(context)?.len() <= 16 * 1024);
        anyhow::ensure!(context["items"].as_array().unwrap().len() <= 8);
        anyhow::ensure!(!context["omissions"].as_array().unwrap().is_empty());
        anyhow::ensure!(context["status"] != "no_indexed_matches");
        anyhow::ensure!(
            context["temporal_scope"]
                .as_str()
                .unwrap()
                .contains("no historical replay cutoff")
        );
        anyhow::ensure!(context["automatic_finding_suppression"] == false);
        anyhow::ensure!(
            response["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["gate"] == "secret_leakage"),
            "Context budget omissions must not consume or suppress current finding slots"
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}


// The public workflow must find explicit decisions even when no review-verdict
// corpus exists, without confusing textual relevance with authority or scope.
async fn public_review_json(engram: &Engram, pid: &str) -> anyhow::Result<Value> {
    let response = engram.pre_commit_review(rmcp::handler::server::tool::Parameters(
        serde_json::from_value(json!({"project_id":pid,"diff":DIFF,
            "output_json":true,"max_findings":100}))?,
    )).await?;
    Ok(serde_json::from_str(text(&response))?)
}

fn standalone_event(id: &str, finding: &str, rationale: &str) -> Value {
    json!({"event_id":id,"finding_id":finding,"supersedes":null,
        "kind":"accepted_exception","source_url":"https://example.invalid/retained-review/100",
        "author":"Declared owner","recorded_at":"2026-01-01T12:00:00Z",
        "rationale":rationale,"verification":null})
}

async fn public_record(engram: &Engram, pid: &str, review_id: &str, events: &[Value]) -> anyhow::Result<()> {
    engram.record_review_decisions(rmcp::handler::server::tool::Parameters(
        serde_json::from_value(json!({"project_id":pid,"review_id":review_id,"decisions":events}))?,
    )).await?;
    Ok(())
}

async fn public_recover(engram: &Engram, pid: &str, review_id: &str) -> anyhow::Result<Value> {
    let response = engram.get_review_decisions(rmcp::handler::server::tool::Parameters(
        serde_json::from_value(json!({"project_id":pid,"review_id":review_id}))?,
    )).await?;
    Ok(serde_json::from_str(text(&response))?)
}

fn assert_current_secret_preserved(response: &Value, baseline: &Value) -> anyhow::Result<()> {
    let finding = |value: &Value| value["findings"].as_array().unwrap().iter()
        .find(|finding|finding["gate"]=="secret_leakage").cloned();
    let before = finding(baseline).ok_or_else(||anyhow::anyhow!("fixture needs a current secret finding: {baseline}"))?;
    let after = finding(response).ok_or_else(||anyhow::anyhow!("earlier decisions must not suppress current finding: {response}"))?;
    anyhow::ensure!(before["severity"] == after["severity"]);
    anyhow::ensure!(response["prior_review_context"]["automatic_finding_suppression"] == false);
    Ok(())
}

#[test]
fn omitted_indexed_review_summaries_are_exactly_recoverable_without_suppressing_findings() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result: anyhow::Result<()> = runtime.block_on(async {
        let (engram, pid, _root) = fixture(temporary.path()).await?;
        let baseline = public_review_json(&engram, &pid).await?;
        let search = engram.state.get_project_cached(&pid).unwrap().search.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        // Stored legacy summaries, not reconstructed original discussions.
        // One long UTF-8 line also requires the returned citation continuation.
        let docs = (1..=13).map(|n| {
            let mut doc = corpus_doc("review_verdict", n);
            doc.generation = 0;
            doc.path = RelPath::new(&format!("review:PR-{}:1", 900 + n));
            doc.content = format!("Stored review summary {n}\r\nFile: /src/export.rs:1\r\nexport password {}",
                "retained UTF-8 detail é. ".repeat(if n == 13 { 4000 } else { 170 }));
            doc.content_hash = ContentHash::compute(doc.content.as_bytes()).0;
            doc
        }).collect::<Vec<_>>();
        search.index_docs(&pid, &docs, &cancel).await?;
        let response = public_review_json(&engram, &pid).await?;
        assert_current_secret_preserved(&response, &baseline)?;
        let context = &response["prior_review_context"];
        anyhow::ensure!(serde_json::to_vec(context)?.len() <= 16 * 1024);
        anyhow::ensure!(context["items"].as_array().unwrap().len() <= 8);
        let omissions = context["omissions"].as_array().unwrap();
        for reason in ["candidate_count_budget", "serialized_response_budget", "complete_stored_document_exceeds_context_budget"] {
            anyhow::ensure!(omissions.iter().any(|o| o["reason"] == reason), "missing exercised omission: {reason}");
        }
        let mut recovered_long_document = false;
        for omission in omissions {
            let recovery = &omission["recovery"];
            anyhow::ensure!(recovery["tool"] == "get_chunk", "omitted indexed summary needs exact recovery: {omission}");
            let mut arguments = recovery["arguments"].clone();
            anyhow::ensure!(arguments["project_id"] == pid && arguments["namespace"] == "review_verdict");
            let original = docs.iter().find(|d| arguments["doc_id"] == d.doc_id)
                .ok_or_else(|| anyhow::anyhow!("recovery lost exact stored document identity"))?;
            anyhow::ensure!(omission["path"] == original.path.as_str());
            let expected_hash = format!("blake3-raw-utf8:{}", blake3::hash(original.content.as_bytes()).to_hex());
            anyhow::ensure!(arguments["citation"]["expected_raw_hash"] == expected_hash);
            let mut complete = String::new();
            let mut pages = 0;
            loop {
                pages += 1;
                anyhow::ensure!(pages <= 4, "citation recovery failed to advance");
                let result = engram.get_chunk(rmcp::handler::server::tool::Parameters(
                    serde_json::from_value(arguments.clone())?,
                )).await?;
                let page: Value = serde_json::from_str(text(&result))?;
                anyhow::ensure!(page["raw_content_hash"] == expected_hash);
                anyhow::ensure!(page["identity"]["doc_id"] == original.doc_id);
                anyhow::ensure!(page["returned_range"]["byte_start"] == complete.len());
                complete.push_str(page["content"].as_str().unwrap());
                if page["continuation"].is_null() { break; }
                arguments["citation"] = page["continuation"].clone();
            }
            anyhow::ensure!(complete.as_bytes() == original.content.as_bytes());
            anyhow::ensure!(format!("blake3-raw-utf8:{}", blake3::hash(complete.as_bytes()).to_hex()) == expected_hash);
            recovered_long_document |= original.content.len() > 40_000 && pages > 1;
        }
        anyhow::ensure!(recovered_long_document);
        // The same action must refuse a replacement under the same doc_id;
        // exact recovery must not silently become a fresh search/current text.
        let stale = omissions[0]["recovery"]["arguments"].clone();
        let mut replacement = docs.iter().find(|d| stale["doc_id"] == d.doc_id).unwrap().clone();
        replacement.content.push_str(" changed after observation");
        replacement.content_hash = ContentHash::compute(replacement.content.as_bytes()).0;
        search.index_docs(&pid, &[replacement], &cancel).await?;
        let rejected = engram.get_chunk(rmcp::handler::server::tool::Parameters(
            serde_json::from_value(stale)?,
        )).await.unwrap_err();
        anyhow::ensure!(rejected.message.contains("citation_hash_mismatch"));
        Ok(())
    });
    drop(runtime);
    let cleanup = temporary.close();
    result?;
    cleanup?;
    Ok(())
}

#[test]
fn ordinary_public_review_discovers_standalone_decisions_preserving_raw_history_and_findings() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result: anyhow::Result<()> = runtime.block_on(async {
        let (engram,pid,root) = fixture(temporary.path()).await?;
        let baseline = public_review_json(&engram,&pid).await?;
        let original = standalone_event("original-exception","thread:1",
            "For /src/export.rs:1, this was a demonstration-only exception. Recheck the production guard and current code.");
        // No ingest_review_verdicts call has occurred: all evidence is recorded
        // through the public decision tool and must be independently discoverable.
        public_record(&engram,&pid,"PR-100", &[original.clone()]).await?;
        public_record(&engram,&pid,"other-folder", &[standalone_event("UnrelatedFolderMarker","thread:2",
            "For other/src/export.rs, the same demonstration-only exception and production guard words apply.")]).await?;
        public_record(&engram,&pid,"longer-path", &[standalone_event("UnrelatedSuffixMarker","thread:3",
            "For src/export.rs.backup, preserve the demonstration-only exception and production guard.")]).await?;
        let response = public_review_json(&engram,&pid).await?;
        assert_current_secret_preserved(&response,&baseline)?;
        let context = &response["prior_review_context"];
        anyhow::ensure!(context["indexed_candidates"] == 0 && context["status"] == "observed", "{context}");
        let items = context["items"].as_array().unwrap();
        anyhow::ensure!(items.len()==1,"{context}");
        let item = &items[0];
        anyhow::ensure!(item["source_kind"] == "standalone_decision_record");
        anyhow::ensure!(item["provider_neutral_events"][0]["provider_identity_authenticated"] == false);
        anyhow::ensure!(item["provider_neutral_events"][0]["current_worktree_and_exception_applicability"] == "not_established");
        anyhow::ensure!(item["recovery"]["arguments"]["review_id"] == "PR-100");
        anyhow::ensure!(!context.to_string().contains("UnrelatedFolderMarker") && !context.to_string().contains("UnrelatedSuffixMarker"));
        let recovered = public_recover(&engram,&pid,"PR-100").await?;
        anyhow::ensure!(recovered["events"] == json!([original]));
        // Canonical event hashes bind the complete event recovered publicly;
        // source array hashes separately identify the observation before append.
        let typed: engram_server::handlers::review_decisions::ReviewDecision =
            serde_json::from_value(recovered["events"][0].clone())?;
        let original_hash = blake3::hash(&serde_json::to_vec(&typed)?).to_hex().to_string();
        anyhow::ensure!(item["recovery"]["event_blake3"] == original_hash);

        let mut superseding = standalone_event("reopened-after-change","thread:1",
            "For src/export.rs, the earlier exception does not certify this changed implementation; retain the new finding.");
        superseding["kind"] = json!("open");
        superseding["supersedes"] = json!("original-exception");
        public_record(&engram,&pid,"PR-100", &[superseding.clone()]).await?;
        let current = public_review_json(&engram,&pid).await?;
        assert_current_secret_preserved(&current,&baseline)?;
        anyhow::ensure!(current["prior_review_context"]["items"].as_array().unwrap().len()==1);
        anyhow::ensure!(current["prior_review_context"]["items"][0]["provider_neutral_events"][0]["evidence"] == superseding);
        let history = public_recover(&engram,&pid,"PR-100").await?;
        anyhow::ensure!(history["events"] == json!([original,superseding]));
        let old: engram_server::handlers::review_decisions::ReviewDecision = serde_json::from_value(history["events"][0].clone())?;
        anyhow::ensure!(blake3::hash(&serde_json::to_vec(&old)?).to_hex().to_string()==original_hash);
        let events_key = format!("review_decisions:{}",blake3::hash(b"PR-100").to_hex());
        let before_idempotent = engram.state.registry.get_meta(&pid,&events_key)?;
        public_record(&engram,&pid,"PR-100", &[original.clone()]).await?;
        anyhow::ensure!(engram.state.registry.get_meta(&pid,&events_key)?==before_idempotent);

        // Once an indexed thread exists, the same immutable event is included
        // once rather than consuming another context slot as a standalone hit.
        ingest(&engram,&pid,&root,&[thread(100,"/src/export.rs",false)]).await?;
        let joined = public_review_json(&engram,&pid).await?;
        let occurrences = joined["prior_review_context"]["items"].as_array().unwrap().iter()
            .filter_map(|item|item["provider_neutral_events"].as_array()).flatten()
            .filter(|entry|entry["evidence"]["event_id"]=="reopened-after-change").count();
        anyhow::ensure!(occurrences==1,"{joined}");
        assert_current_secret_preserved(&joined,&baseline)?;
        let markdown = engram.pre_commit_review(rmcp::handler::server::tool::Parameters(
            serde_json::from_value(json!({"project_id":pid,"diff":DIFF,"output_json":false}))?,
        )).await?;
        anyhow::ensure!(text(&markdown).contains("reopened-after-change") && text(&markdown).contains("Hardcoded secret detected"));
        Ok(())
    });
    drop(runtime);
    let cleanup = temporary.close();
    result?;
    cleanup?;
    Ok(())
}

#[test]
fn public_decision_discovery_discloses_malformed_legacy_and_budget_coverage_without_losing_findings() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result: anyhow::Result<()> = runtime.block_on(async {
        let (engram,pid,_root) = fixture(temporary.path()).await?;
        let baseline = public_review_json(&engram,&pid).await?;
        let legacy = standalone_event("legacy-path-decision","legacy",
            "For src/export.rs, preserve this old observation without guessing its review identity.");
        let legacy_key = format!("review_decisions:{}",blake3::hash(b"legacy-original-id").to_hex());
        let legacy_raw = format!("[\n  {}\n]\n",legacy);
        engram.state.registry.set_meta(&pid,&legacy_key,&legacy_raw)?;
        let before_backfill = public_review_json(&engram,&pid).await?;
        anyhow::ensure!(before_backfill["prior_review_context"]["items"].as_array().unwrap().is_empty());
        anyhow::ensure!(before_backfill["prior_review_context"]["decision_discovery"]["legacy_identity_coverage"].as_str().unwrap().contains("cannot be discovered"));
        anyhow::ensure!(engram.state.registry.get_meta(&pid,&legacy_key)?.as_deref()==Some(legacy_raw.as_str()));
        public_record(&engram,&pid,"legacy-original-id", &[legacy.clone()]).await?;
        anyhow::ensure!(engram.state.registry.get_meta(&pid,&legacy_key)?.as_deref()==Some(legacy_raw.as_str()),
            "Identity backfill must not canonicalize or rewrite the original event array");
        let backfilled = public_review_json(&engram,&pid).await?;
        anyhow::ensure!(backfilled["prior_review_context"]["items"][0]["review_id"]=="legacy-original-id");

        public_record(&engram,&pid,"malformed", &[standalone_event("malformed-record","bad","For src/export.rs, retained observation.")]).await?;
        let malformed_key = format!("review_decisions:{}",blake3::hash(b"malformed").to_hex());
        engram.state.registry.set_meta(&pid,&malformed_key,"not valid JSON")?;
        // A valid public record may exceed the smaller discovery budget. Exact
        // public recovery must still retain its complete events without clipping.
        let large_events = (0..5).map(|n|standalone_event(&format!("large-{n}"),&format!("large-{n}"),
            &format!("For src/export.rs, {}", "bounded retained evidence ".repeat(580)))).collect::<Vec<_>>();
        public_record(&engram,&pid,"large", &large_events).await?;
        let recovered_large = public_recover(&engram,&pid,"large").await?;
        anyhow::ensure!(recovered_large["events"]==json!(large_events));
        // These are deliberately damaged metadata within this owned fixture,
        // not fabricated decoder outcomes or a production injection interface.
        let mismatched = format!("review_decision_identity:{}",blake3::hash(b"wrong-hash").to_hex());
        engram.state.registry.set_meta(&pid,&mismatched,"different-review-id")?;
        let oversized_identity = format!("review_decision_identity:{}",blake3::hash(b"oversized").to_hex());
        engram.state.registry.set_meta(&pid,&oversized_identity,&"x".repeat(20_000))?;
        let response = public_review_json(&engram,&pid).await?;
        let context = &response["prior_review_context"];
        let coverage = &context["decision_discovery"];
        anyhow::ensure!(coverage["invalid_identity_records"]==1,"{coverage}");
        anyhow::ensure!(coverage["identity_values_omitted"]==1,"{coverage}");
        anyhow::ensure!(coverage["unavailable_or_over_budget_records"]==2,"{coverage}");
        anyhow::ensure!(context["status"]=="observed" && context["items"].as_array().unwrap().len()==1);
        anyhow::ensure!(context["items"][0]["review_id"]=="legacy-original-id");
        anyhow::ensure!(serde_json::to_vec(context)?.len()<=16*1024);
        anyhow::ensure!(engram.state.registry.get_meta(&pid,&malformed_key)?.as_deref()==Some("not valid JSON"));
        anyhow::ensure!(engram.state.registry.get_meta(&pid,&mismatched)?.as_deref()==Some("different-review-id"));
        assert_current_secret_preserved(&response,&baseline)?;

        // Several individually admitted arrays also exhaust the aggregate raw
        // observation allowance. Their unrelated bodies must not be copied
        // beyond that allowance just to establish a count-all answer.
        for group in 0..5 {
            let events = (0..4).map(|n|standalone_event(&format!("medium-{group}-{n}"),&format!("medium-{n}"),
                &format!("For src/invoice.rs, {}", "retained unrelated details ".repeat(530)))).collect::<Vec<_>>();
            public_record(&engram,&pid,&format!("medium-{group}"), &events).await?;
        }
        let byte_capped = public_review_json(&engram,&pid).await?;
        let byte_coverage = &byte_capped["prior_review_context"]["decision_discovery"];
        anyhow::ensure!(byte_coverage["unavailable_or_over_budget_records"].as_u64().unwrap()>=3,"{byte_coverage}");
        anyhow::ensure!((200*1024..=256*1024).contains(&byte_coverage["serialized_record_bytes_read"].as_u64().unwrap()),"{byte_coverage}");
        assert_current_secret_preserved(&byte_capped,&baseline)?;
        // A conflicting companion cannot be silently repaired or cause a
        // partial append of new evidence under the wrong identity.
        let legacy_identity = format!("review_decision_identity:{}",blake3::hash(b"legacy-original-id").to_hex());
        engram.state.registry.set_meta(&pid,&legacy_identity,"unrelated-human-value")?;
        let rejected = public_record(&engram,&pid,"legacy-original-id", &[standalone_event("not-published","new",
            "For src/export.rs, this must not partially append.")]).await;
        anyhow::ensure!(rejected.is_err());
        anyhow::ensure!(engram.state.registry.get_meta(&pid,&legacy_key)?.as_deref()==Some(legacy_raw.as_str()));
        let no_valid = public_review_json(&engram,&pid).await?;
        anyhow::ensure!(no_valid["prior_review_context"]["status"]=="decision_evidence_incomplete");
        assert_current_secret_preserved(&no_valid,&baseline)?;

        // Exceed the actual metadata scan bound through public writes; the
        // review must report an uninspected tail rather than count-all or claim
        // no rulings. Shared boilerplate without the changed path stays unrelated.
        for n in 0..65 {
            public_record(&engram,&pid,&format!("unrelated-{n}"), &[standalone_event(&format!("other-{n}"),"other",
                "For src/invoice.rs, the same demonstration-only exception and current production guard words apply.")]).await?;
        }
        let capped = public_review_json(&engram,&pid).await?;
        let coverage = &capped["prior_review_context"]["decision_discovery"];
        anyhow::ensure!(coverage["identity_records_inspected"]==64 && coverage["identity_scan_has_more"]==true,"{coverage}");
        anyhow::ensure!(coverage["serialized_record_bytes_read"].as_u64().unwrap()<=256*1024);
        anyhow::ensure!(capped["prior_review_context"]["status"]=="decision_evidence_incomplete");
        assert_current_secret_preserved(&capped,&baseline)?;
        Ok(())
    });
    drop(runtime);
    let cleanup = temporary.close();
    result?;
    cleanup?;
    Ok(())
}
