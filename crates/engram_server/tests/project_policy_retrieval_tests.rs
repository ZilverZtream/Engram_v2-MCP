#![allow(clippy::unwrap_used)]
//! Wanted public behavior: code context carries the authored guidance for its
//! target, while exact citations and source-freshness refusal keep their meaning.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::Path;

const TARGET: &str = "src/target.rs";
const METHOD: &str = "policyworkflowtarget";
const POLICIES: [(&str, &str); 3] = [
    (
        "AGENTS.md",
        "Preserve accepted behavior and read applicable area instructions.\n",
    ),
    (
        "src/INSTRUCTIONS.md",
        "AREA_GUIDANCE: preserve the accepted UI/server distinction.\n",
    ),
    (
        ".github/instructions/target.instructions.md",
        "---\napplyTo: 'src/**/*.rs'\n---\nACCEPTED_EXCEPTION: UI restrictions do not change authorized server saves.\n",
    ),
];

fn write(root: &Path, path: &str, value: &str) -> Result<()> {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, value)?;
    Ok(())
}

async fn fixture(root: &Path, data: &Path) -> Result<(Engram, String)> {
    fixture_with_sources(root, data, &[]).await
}

async fn fixture_with_sources(
    root: &Path,
    data: &Path,
    extra_sources: &[(String, String)],
) -> Result<(Engram, String)> {
    fixture_with_layout(root, data, extra_sources, true).await
}

async fn fixture_with_layout(
    root: &Path,
    data: &Path,
    extra_sources: &[(String, String)],
    include_primary_area: bool,
) -> Result<(Engram, String)> {
    if include_primary_area {
        write(
            root,
            TARGET,
            "pub fn policyworkflowtarget() -> usize { 7 }\n",
        )?;
    }
    for (path, body) in POLICIES {
        if include_primary_area || path == "AGENTS.md" {
            write(root, path, body)?;
        }
    }
    write(
        root,
        ".github/instructions/billing.instructions.md",
        "---\napplyTo: 'billing/**'\n---\nUNRELATED_BILLING_MUST_NOT_APPLY\n",
    )?;
    {
        let repo = git2::Repository::init(root)?;
        let mut index = repo.index()?;
        index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Owned guidance fixture",
            &tree,
            &[],
        )?;
    }
    // Uncommitted sources are indexed, but have no fabricated co-change signal.
    for (path, body) in extra_sources {
        write(root, path, body)?;
    }
    let (state, _) = AppState::new(Config {
        data_dir: data.into(),
        allowed_roots: vec![root.into()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let engram = Engram::new(state);
    let indexed = engram.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"Policy retrieval fixture","project_type":"rust","wait":true
    }))?).await?;
    let text = &indexed.content[0].as_text().unwrap().text;
    let pid = text
        .split("project_id: ")
        .nth(1)
        .and_then(|text| text.lines().next())
        .ok_or_else(|| anyhow::anyhow!("Missing project identity: {text}"))?
        .trim()
        .to_owned();
    Ok((engram, pid))
}

fn assert_markdown_guidance(text: &str) {
    for (path, body) in POLICIES {
        assert!(
            text.contains(&format!("### `{path}`")),
            "Missing source {path}: {text}"
        );
        assert!(text.contains(&blake3::hash(body.as_bytes()).to_hex().to_string()));
        assert!(
            text.contains(body),
            "Complete qualifications must survive for {path}"
        );
    }
    assert!(!text.contains("UNRELATED_BILLING_MUST_NOT_APPLY"));
}

fn assert_json_guidance(payload: &Value) {
    let policy = &payload["project_policy_sources"];
    assert_eq!(policy["coverage"], "complete");
    let sources = policy["sources"].as_array().unwrap();
    assert_eq!(
        sources.len(),
        POLICIES.len(),
        "Unexpected policy scope: {payload}"
    );
    for (path, body) in POLICIES {
        let source = sources
            .iter()
            .find(|source| source["path"] == path)
            .unwrap();
        assert_eq!(source["content"], body);
        assert_eq!(
            source["blake3"],
            blake3::hash(body.as_bytes()).to_hex().to_string()
        );
    }
}

async fn search(engram: &Engram, pid: &str) -> Result<String> {
    let result = engram
        .handle_search_memory(serde_json::from_value(json!({
            "project_id":pid,"query":METHOD,"semantic":false,"use_mmr":false,
            "include_path_prefixes":[TARGET],"max_results":3
        }))?)
        .await?;
    Ok(result.content[0].as_text().unwrap().text.clone())
}

fn call_arguments(text: &str, label: &str) -> Result<Value> {
    let start = format!("{label}: get_chunk(");
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix(&start))
        .and_then(|line| line.strip_suffix(')'))
        .ok_or_else(|| anyhow::anyhow!("Missing {label}: {text}"))?;
    Ok(serde_json::from_str(json)?)
}

#[test]
fn original_eg01_same_guidance_reaches_planning_editing_retrieval_and_review() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    // The runtime is declared after the directory so unwinding shuts down all
    // blocking workers (and their database handles) before fixture teardown.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("repo");
        let (engram,pid) = fixture(&root,&temporary.path().join("data")).await?;
        // This topic's complete scope excludes the requested source. An
        // unreadable-as-text body must neither consume policy body reads nor
        // downgrade the applicable guidance returned by these public tools.
        let unrelated = root.join(".github/instructions/billing.instructions.md");
        let mut bytes = std::fs::read(&unrelated)?;
        bytes.extend_from_slice(&vec![b'x'; 256 * 1_024]);
        bytes.push(0xff);
        std::fs::write(unrelated, bytes)?;
        let retrieved = search(&engram,&pid).await?;
        assert_markdown_guidance(&retrieved);
        ensure!(retrieved.contains("Status: present; coverage: complete."),
            "An excluded topic body cannot make applicable policy coverage incomplete: {retrieved}");
        let chunk_args = call_arguments(&retrieved,"full_chunk")?;
        assert_eq!(chunk_args["inject_rules"],true,"Advertised source recovery must include guidance automatically");
        let chunk = engram.handle_get_chunk(serde_json::from_value(chunk_args)?).await?;
        assert_markdown_guidance(&chunk.content[0].as_text().unwrap().text);
        for output_json in [true,false] {
            let planned = engram.handle_get_change_set(serde_json::from_value(json!({
                "project_id":pid,"story":"Change policyworkflowtarget while preserving accepted behavior",
                "concepts":[METHOD],"output_json":output_json
            }))?).await?;
            let prepared = engram.handle_prepare_implementation_context(serde_json::from_value(json!({
                "project_id":pid,"file_path":TARGET,"method_name":METHOD,
                "include_style_profile":false,"output_json":output_json
            }))?).await?;
            let diff = format!("diff --git a/{TARGET} b/{TARGET}\n--- a/{TARGET}\n+++ b/{TARGET}\n@@ -1 +1 @@\n-pub fn policyworkflowtarget() -> usize {{ 7 }}\n+pub fn policyworkflowtarget() -> usize {{ 8 }}\n");
            let gates = engram_server::services::pre_commit_review_service::all_gates().iter()
                .map(|gate|gate.name().to_string()).collect::<Vec<_>>();
            let reviewed = engram.handle_pre_commit_review(serde_json::from_value(json!({
                "project_id":pid,"diff":diff,"skip_gates":gates,"output_json":output_json
            }))?).await?;
            for response in [planned,prepared,reviewed] {
                let text = &response.content[0].as_text().unwrap().text;
                if output_json { assert_json_guidance(&serde_json::from_str(text)?); }
                else { assert_markdown_guidance(text); }
            }
        }
        Ok(())
    });
    drop(runtime);
    temporary.close()?;
    result
}

#[test]
fn original_eg01_guidance_does_not_change_citations_or_weaken_missing_source_refusal() -> Result<()>
{
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("repo");
        let (engram, pid) = fixture(&root, &temporary.path().join("data")).await?;
        let retrieved = search(&engram, &pid).await?;
        let citation_args = call_arguments(&retrieved, "citation_chunk")?;
        ensure!(citation_args.get("inject_rules").is_none());
        let before = engram
            .handle_get_chunk(serde_json::from_value(citation_args.clone())?)
            .await?;
        let before: Value = serde_json::from_str(&before.content[0].as_text().unwrap().text)?;
        ensure!(before["content"].as_str().unwrap().contains(METHOD));
        ensure!(
            !before["content"]
                .as_str()
                .unwrap()
                .contains("ACCEPTED_EXCEPTION")
        );
        write(
            &root,
            "src/INSTRUCTIONS.md",
            "UPDATED_AREA_GUIDANCE: preserve the same accepted exception.\n",
        )?;
        let refreshed = search(&engram, &pid).await?;
        ensure!(refreshed.contains("UPDATED_AREA_GUIDANCE"));
        let after = engram
            .handle_get_chunk(serde_json::from_value(citation_args)?)
            .await?;
        let after: Value = serde_json::from_str(&after.content[0].as_text().unwrap().text)?;
        ensure!(
            after["content"] == before["content"]
                && after["raw_content_hash"] == before["raw_content_hash"]
        );
        let mut plain = call_arguments(&refreshed, "full_chunk")?;
        plain["inject_rules"] = json!(false);
        let without = engram
            .handle_get_chunk(serde_json::from_value(plain.clone())?)
            .await?;
        ensure!(
            !without.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("UPDATED_AREA_GUIDANCE")
        );
        std::fs::remove_file(root.join(TARGET))?;
        let refused = engram
            .handle_get_chunk(serde_json::from_value(plain)?)
            .await;
        ensure!(
            refused.is_err(),
            "Missing checkout source must not become editable cached code"
        );
        Ok(())
    });
    drop(runtime);
    temporary.close()?;
    result
}

async fn recover_dossier(engram: &Engram, manifest: &Value) -> Result<String> {
    let id = manifest["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    let mut next = manifest["response_delivery"]["next_call"].clone();
    let mut original = String::new();
    for _ in 0..64 {
        ensure!(next["tool"] == "get_change_set");
        let response = engram
            .handle_get_change_set(serde_json::from_value(next["arguments"].clone())?)
            .await?;
        let text = &response.content[0].as_text().unwrap().text;
        ensure!(
            text.len() <= 40 * 1024,
            "The entire serialized page must fit"
        );
        let page: Value = serde_json::from_str(text)?;
        ensure!(page["response_id"] == id);
        ensure!(page["start_byte"] == original.len());
        let previous = original.len();
        original.push_str(page["content"].as_str().unwrap());
        ensure!(page["end_byte"] == original.len());
        if page["complete"] == true {
            ensure!(blake3::hash(original.as_bytes()).to_hex().as_str() == id);
            return Ok(original);
        }
        ensure!(original.len() > previous, "Continuation must make progress");
        next = page["next_call"].clone();
    }
    anyhow::bail!("Owned bounded dossier recovery did not finish")
}

#[test]
fn original_eg01_root_and_spaced_source_paths_retain_their_authored_guidance() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("repo");
        let policy_path = ".github/instructions/root-and-spaces.instructions.md";
        let policy_body = "---\napplyTo: ['main.rs', 'feature area/root item.rs']\n---\nROOT_SCOPE_EXCEPTION: retain complete qualifications even for root files and spaced paths.\n";
        write(&root, policy_path, policy_body)?;
        let cases = [("main.rs", "rootpolicytarget"), ("feature area/root item.rs", "spacedpolicytarget")];
        let sources = cases.iter().map(|(path, method)| (path.to_string(), format!("pub fn {method}() -> usize {{ 7 }}\n"))).collect::<Vec<_>>();
        let (engram, pid) = fixture_with_layout(&root, &temporary.path().join("data"), &sources, false).await?;
        for (path, method) in cases {
            let footprint = engram.handle_get_concept_footprint(serde_json::from_value(json!({
                "project_id":pid,"concept":method,"max_per_group":12
            }))?).await?;
            ensure!(footprint.content[0].as_text().unwrap().text.contains(path), "The source must first be discoverable by the public concept tool");
            let result = engram.handle_get_change_set(serde_json::from_value(json!({
                "project_id":pid,"story":format!("Change {method} while preserving accepted behavior"),
                "concepts":[method],"output_json":true
            }))?).await?;
            let payload: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text)?;
            ensure!(payload["files"].as_array().unwrap().iter().any(|file|file["path"] == path), "Planning lost the exact source identity {path}: {payload}");
            let policy = &payload["project_policy_sources"];
            ensure!(policy["coverage"] == "complete");
            let sources = policy["sources"].as_array().unwrap();
            ensure!(sources.len() == 2, "Only root and the matching topic apply: {policy}");
            let topic = sources.iter().find(|source|source["path"] == policy_path).unwrap();
            ensure!(topic["content"] == policy_body);
            ensure!(topic["blake3"] == blake3::hash(policy_body.as_bytes()).to_hex().to_string());
        }
        Ok(())
    });
    drop(runtime);
    temporary.close()?;
    result
}

#[test]
fn original_eg02_large_public_dossiers_keep_omissions_and_immutable_recovery_in_both_formats()
-> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("repo");
        let concepts = ["dossieralpha", "dossierbeta", "dossiergamma"];
        let mut sources = Vec::new();
        for concept in concepts {
            for index in 0..20 {
                let (path, name) = if index < 12 {
                    (
                        format!("src/{concept}/item{index}.rs"),
                        format!("{concept}_{index}"),
                    )
                } else {
                    (
                        format!("src/{concept}/{concept}_extra{index}.rs"),
                        format!("other_{index}"),
                    )
                };
                sources.push((path, format!("pub fn {name}() -> usize {{ {index} }}\n")));
            }
        }
        let (engram, pid) =
            fixture_with_sources(&root, &temporary.path().join("data"), &sources).await?;
        let story = format!(
            "Change `{TARGET}` and `{METHOD}()` while preserving accepted behavior. {}",
            "Preserve å🦀, \"quoted\" text, backslash \\ and qualifications.\n".repeat(1_000)
        );
        let mut first_summary = None;
        let mut first_receipt = None;
        for output_json in [true, false] {
            write(&root, POLICIES[1].0, POLICIES[1].1)?;
            let response = engram
                .handle_get_change_set(serde_json::from_value(json!({
                    "project_id":pid,"story":story,"concepts":concepts,"output_json":output_json
                }))?)
                .await?;
            let text = &response.content[0].as_text().unwrap().text;
            ensure!(
                text.len() <= 40 * 1024,
                "The entire public dossier must fit"
            );
            let manifest: Value = serde_json::from_str(if output_json {
                text
            } else {
                text.split_once("```json\n")
                    .unwrap()
                    .1
                    .split_once("\n```")
                    .unwrap()
                    .0
            })?;
            ensure!(manifest["response_delivery"]["status"] == "DEFERRED_CONTEXT");
            ensure!(manifest["response_delivery"]["complete"] == false);
            let omitted = manifest["omission_summary"]["candidate_rows_omitted"]
                .as_u64()
                .unwrap();
            ensure!(
                omitted > 20,
                "Fixture must exercise omission details beyond the compact cap: {manifest}"
            );
            ensure!(manifest["view"]["omissions_total"] == omitted);
            let reason_counts = manifest["omission_summary"]["by_reason"]
                .as_object()
                .unwrap();
            ensure!(
                reason_counts
                    .values()
                    .map(|count| count.as_u64().unwrap())
                    .sum::<u64>()
                    == omitted
            );
            ensure!(
                reason_counts
                    .keys()
                    .all(|reason| reason.contains("tail cap"))
            );
            let rows = manifest["reconciliation_rows"].as_array().unwrap();
            ensure!(
                !rows.is_empty(),
                "Required reconciliation identities must be exercised"
            );
            ensure!(rows.iter().any(|row| row["path"] == TARGET));
            ensure!(manifest["reconciliation"]["required_rows"] == rows.len());
            if let Some(summary) = &first_summary {
                ensure!(summary == &manifest["omission_summary"]);
            }
            if let Some(receipt) = &first_receipt {
                ensure!(receipt == &manifest["reconciliation"]);
            }
            first_summary = Some(manifest["omission_summary"].clone());
            first_receipt = Some(manifest["reconciliation"].clone());

            write(&root, POLICIES[1].0, "MUTATED_POLICY_AFTER_SNAPSHOT\n")?;
            let original = recover_dossier(&engram, &manifest).await?;
            ensure!(original.contains("AREA_GUIDANCE") && original.contains("ACCEPTED_EXCEPTION"));
            ensure!(!original.contains("MUTATED_POLICY_AFTER_SNAPSHOT"));
            if output_json {
                let recovered: Value = serde_json::from_str(&original)?;
                ensure!(recovered["reconciliation"] == manifest["reconciliation"]);
                ensure!(recovered["view"] == manifest["view"]);
                ensure!(recovered["omission_summary"] == manifest["omission_summary"]);
                let shown = recovered["omissions"].as_array().unwrap().len();
                ensure!(
                    recovered["view"]["omissions_omitted_from_view"]
                        .as_u64()
                        .unwrap()
                        == omitted - shown as u64
                );
                ensure!(shown > 0 && (shown as u64) < omitted);
            } else {
                ensure!(
                    original.contains(manifest["reconciliation"]["receipt_id"].as_str().unwrap())
                );
                for row in rows {
                    ensure!(original.contains(&format!("{}.", row["row_id"].as_str().unwrap())));
                }
                ensure!(original.contains(&format!("{omitted} weak-signal candidate(s) omitted")));
            }
        }
        Ok(())
    });
    drop(runtime);
    temporary.close()?;
    result
}
