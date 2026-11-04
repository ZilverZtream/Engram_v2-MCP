//! A user's Git revert is a new source state: index it, reopen, then keep editing.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_index::HybridQuery;
use engram_server::{Engram, state::AppState};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use tokio_util::sync::CancellationToken;

fn commit(root: &Path, message: &str) -> Result<git2::Oid> {
    let repo = git2::Repository::open(root)?;
    let mut index = repo.index()?;
    index.add_all(["src"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
    let parent = repo.head().ok().map(|head| head.peel_to_commit()).transpose()?;
    Ok(repo.commit(Some("HEAD"), &signature, &signature, message, &tree,
        &parent.iter().collect::<Vec<_>>())?)
}

fn generation(engram: &Engram, pid: &str) -> Result<u64> {
    Ok(engram.state.registry.get_meta(pid, "active_generation")?
        .ok_or_else(|| anyhow::anyhow!("Published generation missing"))?.parse()?)
}

fn query(pid: &str, generation: u64, marker: &str) -> HybridQuery {
    HybridQuery { project_id: pid.into(), namespace: "memory".into(), generation,
        text: marker.into(), top_k: 100, fts_mode: "strict".into(),
        include_path_prefixes: None, exclude_path_prefixes: None,
        include_path_suffixes: None, language_filters: None, author_filter: None,
        date_after: None, date_before: None, use_mmr: false }
}

fn content_and_identity(engram: &Engram, pid: &str, marker: &str) -> Result<(String, String)> {
    let rt = engram.state.get_project_cached(pid).ok_or_else(|| anyhow::anyhow!("Runtime missing"))?;
    let generation = generation(engram, pid)?;
    let hits = rt.search.lexical_search(&query(pid, generation, marker))?;
    ensure!(hits.len() == 1, "Expected exactly one source declaration for {marker}: {hits:?}");
    let hit = &hits[0];
    let (path, _, content, start, end) = rt.search.get_doc_by_doc_id(pid, "memory", generation, &hit.doc_id)?
        .ok_or_else(|| anyhow::anyhow!("Source citation unavailable"))?;
    ensure!(path == hit.path && start == hit.start_line && end == hit.end_line && start > 0 && end >= start,
        "Citation identity/line bounds differ from source result");
    ensure!(content.contains(marker), "Recovered source omitted the declaration");
    Ok((blake3::hash(content.as_bytes()).to_hex().to_string(), hit.pk.clone()))
}

fn absent(engram: &Engram, pid: &str, marker: &str) -> Result<()> {
    let rt = engram.state.get_project_cached(pid).ok_or_else(|| anyhow::anyhow!("Runtime missing"))?;
    ensure!(rt.search.lexical_search(&query(pid, generation(engram, pid)?, marker))?.is_empty(),
        "Obsolete source declaration remained visible: {marker}");
    Ok(())
}

async fn verify_paths_and_vectors(engram: &Engram, pid: &str, paths: &[&str]) -> Result<()> {
    let rt = engram.state.get_project_cached(pid).ok_or_else(|| anyhow::anyhow!("Runtime missing"))?;
    let generation = generation(engram, pid)?;
    let expected: BTreeSet<String> = paths.iter().map(|p| p.to_string()).collect();
    ensure!(rt.search.paths_in_generation(pid, Some("memory"), generation)? == expected);
    #[cfg(feature = "vector")]
    {
        ensure!(rt.search.vector_paths_in_generation(pid, Some("memory"), generation).await? == expected);
        ensure!(rt.search.count_vectors_in_generation(pid, generation).await?
            == rt.search.count_docs_in_generation(pid, "memory", generation)?, "Vector coverage differs from source coverage");
        let hits = rt.search.vector_search(&query(pid, generation, "rollback source"), &CancellationToken::new()).await?;
        ensure!(!hits.is_empty() && hits.iter().map(|h| h.path.as_str().to_string()).collect::<BTreeSet<_>>() == expected,
            "Native query returned missing, deleted or obsolete source paths");
    }
    Ok(())
}

async fn update(engram: &Engram, pid: &str) -> Result<()> {
    let result = engram.handle_update_project(serde_json::from_value(json!({
        "project_id":pid,"wait":true,"index_antipatterns":false
    }))?).await?;
    ensure!(result.is_error != Some(true), "Public source update failed: {result:?}");
    Ok(())
}

#[tokio::test]
async fn git_revert_then_reopen_and_another_edit_preserves_the_complete_source_view() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        git2::Repository::init(&root)?;
        let hooks = root.join(".git/fixture-empty-hooks");
        std::fs::create_dir(&hooks)?;
        std::fs::write(root.join("src/target.rs"), "pub fn rollbackastate() -> usize { 1 }\n")?;
        std::fs::write(root.join("src/companion.rs"), "pub fn stablecompanion() -> usize { 7 }\n")?;
        commit(&root, "state A")?;
        let cfg = Config { data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: if cfg!(feature = "vector") { "local" } else { "fts_only" }.into(),
            ..Default::default() };
        let (pid, original_content_hash, companion_identity, reverted_generation) = {
            let (state, _) = AppState::new(cfg.clone())?;
            let engram = Engram::new(state);
            let indexed = engram.handle_index_project(serde_json::from_value(json!({
                "directory":root,"project_name":"Git revert source continuity","project_type":"rust","wait":true
            }))?).await?;
            ensure!(indexed.is_error != Some(true), "Initial source indexing failed");
            let text = &indexed.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing index response"))?.text;
            let pid = text.split("project_id: ").nth(1).and_then(|s| s.lines().next())
                .ok_or_else(|| anyhow::anyhow!("Missing project ID"))?.trim().to_string();
            let a = generation(&engram, &pid)?;
            let original = content_and_identity(&engram, &pid, "rollbackastate")?;
            let companion = content_and_identity(&engram, &pid, "stablecompanion")?;
            verify_paths_and_vectors(&engram, &pid, &["src/target.rs", "src/companion.rs"]).await?;

            std::fs::write(root.join("src/target.rs"), "pub fn rollbackbstate() -> usize { 2 }\n")?;
            std::fs::write(root.join("src/bonly.rs"), "pub fn rollbackbonly() -> usize { 9 }\n")?;
            let b_commit = commit(&root, "state B")?;
            update(&engram, &pid).await?;
            let b = generation(&engram, &pid)?;
            ensure!(b > a);
            absent(&engram, &pid, "rollbackastate")?;
            content_and_identity(&engram, &pid, "rollbackbstate")?;
            content_and_identity(&engram, &pid, "rollbackbonly")?;
            ensure!(content_and_identity(&engram, &pid, "stablecompanion")? == companion);
            verify_paths_and_vectors(&engram, &pid, &["src/target.rs", "src/companion.rs", "src/bonly.rs"]).await?;

            // Actual Git revert, with signing and external hooks disabled only
            // for this one owned-fixture command. No global Git settings change.
            let reverted = std::process::Command::new("git").arg("-C").arg(&root)
                .args(["-c", "commit.gpgsign=false", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c"])
                .arg(format!("core.hooksPath={}", hooks.display()))
                .args(["revert", "--no-edit", &b_commit.to_string()]).output()?;
            ensure!(reverted.status.success(), "Owned Git revert failed: {}", String::from_utf8_lossy(&reverted.stderr));
            ensure!(!root.join("src/bonly.rs").exists());
            update(&engram, &pid).await?;
            let reverted_generation = generation(&engram, &pid)?;
            ensure!(reverted_generation > b, "A Git revert must publish a new source view");
            ensure!(content_and_identity(&engram, &pid, "rollbackastate")?.0 == original.0);
            absent(&engram, &pid, "rollbackbstate")?;
            absent(&engram, &pid, "rollbackbonly")?;
            ensure!(content_and_identity(&engram, &pid, "stablecompanion")? == companion);
            verify_paths_and_vectors(&engram, &pid, &["src/target.rs", "src/companion.rs"]).await?;
            (pid, original.0, companion, reverted_generation)
        };
        // All first-engine readers and runtime handles have left scope.
        {
            let (state, _) = AppState::new(cfg)?;
            let engram = Engram::new(state);
            update(&engram, &pid).await?;
            ensure!(generation(&engram, &pid)? == reverted_generation, "Cold unchanged update rewrote the source view");
            ensure!(content_and_identity(&engram, &pid, "rollbackastate")?.0 == original_content_hash);
            std::fs::write(root.join("src/target.rs"), "pub fn rollbackcstate() -> usize { 3 }\n")?;
            commit(&root, "state C after revert")?;
            update(&engram, &pid).await?;
            ensure!(generation(&engram, &pid)? > reverted_generation);
            content_and_identity(&engram, &pid, "rollbackcstate")?;
            for marker in ["rollbackastate", "rollbackbstate", "rollbackbonly"] { absent(&engram, &pid, marker)?; }
            ensure!(content_and_identity(&engram, &pid, "stablecompanion")? == companion_identity);
            verify_paths_and_vectors(&engram, &pid, &["src/target.rs", "src/companion.rs"]).await?;
        }
        Ok(())
    }.await;
    temporary.close()?;
    result
}
