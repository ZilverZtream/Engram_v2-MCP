//! Repair's source work and requested Git follow-up have separate contracts.
use super::*;
use anyhow::{Result, ensure};
use git2::{Oid, Repository, Signature};
use std::collections::BTreeSet;
use std::time::Duration;

const LATEST: &str = "pub fn repair_option_marker() -> u32 { 1 }\n";
const WATERMARKS: [&str; 4] = [
    "last_git_oid", "oldest_indexed_git_oid", "git_backfill_complete", "pr_ingest_watermark",
];

fn commit(repo: &Repository, text: &str, message: &str) -> Result<Oid> {
    std::fs::write(repo.workdir().unwrap().join("source.rs"), text)?;
    let mut index = repo.index()?;
    index.add_path(Path::new("source.rs"))?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    let author = Signature::now("Owned repair options", "fixture@example.invalid")?;
    Ok(repo.commit(Some("HEAD"), &author, &author, message, &tree, &parents)?)
}

async fn seed(temp: &Path) -> Result<(Engram, [Oid; 3])> {
    // An absent HEAD is intentional: the actual complete legacy producer below
    // attests absence only after indexing the whole admitted source file.
    let app = fixture_at_with_initial_head(temp, "fts_only", false);
    let root = temp.join("repo");
    let git = Repository::open(&root)?;
    let first = commit(&git, LATEST, "repair options baseline")?;
    let bad = commit(&git, "pub fn repair_option_marker() -> u32 { 99 }\n", "repair options regression")?;
    let tip = commit(&git, LATEST, &format!("Revert regression\n\nThis reverts commit {bad}."))?;
    drop(git);
    let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let stats = ps.search.index_files(PROJECT, "memory", 1, &root,
        vec![root.join("source.rs")], app.state.cfg.max_chunks_per_file, &CancellationToken::new(), |_, _| {}).await?;
    app.process_ingest_stats(PROJECT, 1, &stats).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let legacy = app.state.registry.capture_source_head(PROJECT)?;
    ensure!(app.state.registry.attest_legacy_source_head(&legacy)?);
    drop(ps);
    Ok((app, [first, bad, tip]))
}

async fn repair(app: &Engram, scope: &str, cap: usize, antipatterns: bool) -> Result<String> {
    let result = app.handle_repair_project(serde_json::from_value(json!({
        "project_id": PROJECT, "scope": scope, "wipe_and_reindex": false,
        "max_commits": cap, "index_antipatterns": antipatterns
    }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(result.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing repair response"))?.text.clone())
}

async fn complete_source(app: &Engram) -> Result<()> {
    let generation = app.state.registry.active_source_generation(PROJECT)?;
    let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let view = ps.search.pin_source_generation(PROJECT, generation)?;
    let hits = ps.search.lexical_search(&query(generation, "repair_option_marker"))?;
    ensure!(hits.len() == 1 && ps.search.count_docs_in_generation(PROJECT, "memory", generation)? == 1,
        "Repair must publish the complete known source file: {hits:?}");
    let (path, _, text, first, last) = ps.search.recover_selected_source_hit(PROJECT, &view, &hits[0])?
        .ok_or_else(|| anyhow::anyhow!("Missing repaired source body"))?;
    ensure!(path.as_str() == "source.rs" && first == 1 && last == 1 && text == LATEST,
        "Repair published unexpected source: {path:?} {first}..{last} {text:?}");
    Ok(())
}

// The wrapper owns the runtime AFTER the TempDir. Unwinding therefore drops
// async tasks and drains their blocking work before the owned directory is removed.
#[test]
fn zero_git_cap_repairs_source_without_jobs_history_policy_or_watermark_writes() -> Result<()> {
    for scope in ["full", "graph_only"] {
        let temp = tempfile::tempdir()?;
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        runtime.block_on(async {
            let (app, _) = seed(temp.path()).await?;
            let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
            let mut sentinels = Vec::new();
            for namespace in ["history", "antipattern"] {
                let mut doc = old_doc();
                doc.namespace = namespace.into();
                doc.generation = 0;
                doc.doc_id = format!("repair-options-{namespace}");
                doc.path = RelPath::new(&format!("retained:{namespace}"));
                doc.content = format!("retained complete {namespace} evidence");
                doc.content_hash = ContentHash::compute(doc.content.as_bytes()).0;
                sentinels.push(doc);
            }
            for doc in &sentinels {
                ps.search.index_docs(PROJECT, std::slice::from_ref(doc), &CancellationToken::new()).await?;
            }
            app.state.registry.put_repo_rule(PROJECT, &engram_core::RepoRule {
                rule_id: "repair-options-manual".into(), file_pattern: "*.rs".into(),
                rule_text: "Preserve this explicit manual rule during source-only maintenance.".into(),
                priority: 17, updated_at_ms: 42, introduced_at: None, provenance: None,
            })?;
            let rules = app.state.registry.list_repo_rules(PROJECT)?;
            for key in WATERMARKS {
                app.state.registry.set_meta(PROJECT, key, &format!("retained-{key}"))?;
            }
            ensure!(app.state.registry.list_jobs(Some(PROJECT))?.is_empty());
            let response = repair(&app, scope, 0, true).await?;
            // Check the durable job list as well as currently running owners:
            // a fast completed writer must not evade the zero-work contract.
            finish_owned_jobs(&app).await;
            ensure!(response.contains("skipped") && response.contains(if scope == "graph_only" { "graph_only" } else { "max_commits=0" }), "{scope}: {response}");
            ensure!(app.state.registry.list_jobs(Some(PROJECT))?.is_empty(),
                "{scope}: zero-cap repair created a durable Git job");
            ensure!(app.state.active_jobs.read().await.is_empty());
            ensure!(app.state.registry.list_repo_rules(PROJECT)? == rules,
                "{scope}: source-only repair changed policy rows");
            for key in WATERMARKS {
                ensure!(app.state.registry.get_meta(PROJECT, key)?.as_deref() == Some(format!("retained-{key}").as_str()),
                    "{scope}: source-only repair changed {key}");
            }
            for doc in &sentinels {
                let rows = ps.search.list_docs_in_namespace(PROJECT, &doc.namespace)?;
                ensure!(rows.len() == 1 && rows[0].doc_id == doc.doc_id && rows[0].path == doc.path.as_str(),
                    "{scope}: source-only repair changed {} rows: {rows:?}", doc.namespace);
                let body = ps.search.get_doc_by_doc_id(PROJECT, &doc.namespace, 0, &doc.doc_id)?
                    .ok_or_else(|| anyhow::anyhow!("Missing preserved {} body", doc.namespace))?;
                ensure!(body.2 == doc.content, "{scope}: source-only repair changed {}", doc.namespace);
            }
            complete_source(&app).await?;
            drop(ps); drop(app);
            Ok::<_, anyhow::Error>(())
        })?;
        drop(runtime);
        temp.close()?;
    }
    Ok(())
}

#[test]
fn requested_one_commit_cap_and_antipattern_switch_control_actual_git_followup() -> Result<()> {
    for scope in ["full"] {
        for enabled in [false, true] {
            let temp = tempfile::tempdir()?;
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(async {
                let (app, [first, bad, tip]) = seed(temp.path()).await?;
                let response = repair(&app, scope, 1, enabled).await?;
                ensure!(response.contains("git history re-walk started"), "{response}");
                tokio::time::timeout(Duration::from_secs(30), finish_owned_jobs(&app)).await?;
                let jobs = app.state.registry.list_jobs(Some(PROJECT))?;
                ensure!(jobs.len() == 1 && jobs[0].kind == "index_git_history" && jobs[0].status == "done",
                    "{scope}, antipatterns={enabled}: {jobs:?}");
                ensure!(jobs[0].message.contains("commits_processed: 1") && jobs[0].message.contains("max_commits cap reached"),
                    "{scope}, antipatterns={enabled}: {}", jobs[0].message);
                let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
                let history = ps.search.list_docs_in_namespace(PROJECT, "history")?;
                let paths: BTreeSet<_> = history.iter().map(|row| row.path.as_str()).collect();
                let expected = BTreeSet::from([format!("commit:{tip}"), format!("diff:{tip}:source.rs")]);
                ensure!(paths == expected.iter().map(String::as_str).collect(),
                    "{scope}, antipatterns={enabled}: expected tip only, history={history:?}; older={first},{bad}");
                ensure!(app.state.registry.get_meta(PROJECT, "last_git_oid")?.as_deref() == Some(tip.to_string().as_str()));
                ensure!(app.state.registry.get_meta(PROJECT, "oldest_indexed_git_oid")?.as_deref() == Some(tip.to_string().as_str()));
                ensure!(app.state.registry.get_meta(PROJECT, "git_backfill_complete")?.unwrap_or_default().is_empty(),
                    "A one-commit repair must not claim the three-commit history is complete");
                let antipatterns = ps.search.list_docs_in_namespace(PROJECT, "antipattern")?;
                ensure!(antipatterns.len() == usize::from(enabled),
                    "{scope}, antipatterns={enabled}: actual retained rows={antipatterns:?}");
                if enabled {
                    let generation = app.state.registry.active_source_generation(PROJECT)?;
                    let (_, _, body, _, _) = ps.search.get_doc_by_doc_id(PROJECT, "antipattern", generation, &antipatterns[0].doc_id)?
                        .ok_or_else(|| anyhow::anyhow!("Missing actual revert evidence"))?;
                    ensure!(body.contains(&format!("Original Commit: {bad}")) &&
                        body.contains(&format!("Reverted in Commit: {tip}")) && body.contains("+pub fn repair_option_marker() -> u32 { 99 }"),
                        "Antipattern evidence must name the actual original/tip and original diff: {body}");
                }
                complete_source(&app).await?;
                drop(ps); drop(app);
                Ok::<_, anyhow::Error>(())
            })?;
            drop(runtime);
            temp.close()?;
        }
    }
    Ok(())
}
