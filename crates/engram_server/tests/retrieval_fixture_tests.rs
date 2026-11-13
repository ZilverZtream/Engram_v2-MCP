//! Fixed lexical retrieval answers for authored WebForms workflows.
//! This is a search regression gate, not a semantic/model or productivity score.
use anyhow::{Context, Result, ensure};
use engram_core::{Config, benchmark::BenchmarkQueryEntry};
use engram_server::services::benchmark_service::{compute_recall, compute_reciprocal_rank};
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    schema_version: u32,
    name: String,
    description: String,
    k: usize,
    candidate_chunks: usize,
    min_mean_reciprocal_rank: f64,
    min_mean_precision_at_k: f64,
    require_all_relevant_paths: bool,
    source_files: Vec<String>,
    queries: Vec<BenchmarkQueryEntry>,
}

/// Own the async runtime too: on assertion failure it still drains index jobs,
/// closes database owners and waits for blocking workers before temp teardown.
struct Fixture {
    server: Option<Engram>,
    runtime: Option<tokio::runtime::Runtime>,
    temporary: Option<tempfile::TempDir>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Some(server), Some(runtime)) = (&self.server, &self.runtime) {
            runtime.block_on(async {
                loop {
                    let jobs: Vec<_> = server
                        .state
                        .active_jobs
                        .write()
                        .await
                        .drain()
                        .map(|(_, task)| task)
                        .collect();
                    if jobs.is_empty() {
                        break;
                    }
                    for job in jobs {
                        let _finished = job.await;
                    }
                }
            });
        }
        drop(self.server.take());
        drop(self.runtime.take());
        if let Some(temporary) = self.temporary.take() {
            let path = temporary.path().to_owned();
            if let Err(error) = temporary.close() {
                if std::thread::panicking() {
                    eprintln!(
                        "Owned retrieval fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                } else {
                    panic!(
                        "Owned retrieval fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
}

fn text(reply: CallToolResult) -> Result<String> {
    ensure!(reply.is_error != Some(true), "Tool error: {reply:?}");
    Ok(reply
        .content
        .iter()
        .filter_map(|item| item.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn distinct_paths(paths: impl IntoIterator<Item = String>, k: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .take(k)
        .collect()
}

fn score(paths: &[String], expected: &[String], k: usize) -> Value {
    let found = paths
        .iter()
        .take(k)
        .filter(|p| expected.contains(p))
        .count();
    json!({
        "actual_paths": paths,
        "expected_paths": expected,
        // Fixed denominator: short/empty result sets do not inflate precision.
        "precision_at_k": found as f64 / k as f64,
        "recall_at_k": compute_recall(paths, expected, k),
        "reciprocal_rank": compute_reciprocal_rank(paths, expected),
    })
}

fn passes(rows: &[Value], expected: &Expected) -> bool {
    if rows.len() != expected.queries.len() || rows.is_empty() {
        return false;
    }
    let mean = |key: &str| {
        rows.iter()
            .map(|r| r[key].as_f64().unwrap_or(0.0))
            .sum::<f64>()
            / rows.len() as f64
    };
    (!expected.require_all_relevant_paths
        || rows.iter().all(|r| r["recall_at_k"].as_f64() == Some(1.0)))
        && mean("precision_at_k") + f64::EPSILON >= expected.min_mean_precision_at_k
        && mean("reciprocal_rank") + f64::EPSILON >= expected.min_mean_reciprocal_rank
}

fn oracle() -> Result<(PathBuf, Expected)> {
    let root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/webforms_retrieval");
    let expected: Expected = serde_json::from_slice(&std::fs::read(root.join("expected.json"))?)?;
    ensure!(
        expected.schema_version == 1 && expected.k == 5 && expected.candidate_chunks >= expected.k
    );
    ensure!(!expected.queries.is_empty() && !expected.description.is_empty());
    let mut ids = BTreeSet::new();
    for query in &expected.queries {
        ensure!(
            ids.insert(&query.query_id) && !query.query.is_empty() && !query.rationale.is_empty()
        );
        ensure!(!query.relevant_paths.is_empty());
        for path in &query.relevant_paths {
            ensure!(
                expected.source_files.contains(path),
                "Unknown oracle file: {path}"
            );
        }
    }
    Ok((root, expected))
}

fn commit(root: &Path) -> Result<()> {
    let repo = git2::Repository::init(root)?;
    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let who = git2::Signature::now("Owned Fixture", "fixture@example.invalid")?;
    repo.commit(
        Some("HEAD"),
        &who,
        &who,
        "Authored retrieval source",
        &tree,
        &[],
    )?;
    Ok(())
}

#[test]
fn authored_webforms_queries_recover_expected_source() -> Result<()> {
    let (corpus, expected) = oracle()?;
    let mut fixture = Fixture {
        server: None,
        runtime: None,
        temporary: Some(tempfile::tempdir()?),
    };
    let owned = fixture
        .temporary
        .as_ref()
        .context("Temporary owner missing")?
        .path()
        .to_owned();
    let root = owned.join("source");
    std::fs::create_dir(&root)?;
    for name in &expected.source_files {
        ensure!(!name.contains("..") && !Path::new(name).is_absolute());
        let destination = root.join(name);
        std::fs::create_dir_all(destination.parent().context("Source parent missing")?)?;
        std::fs::copy(corpus.join("source").join(name), destination)?;
    }
    commit(&root)?;
    fixture.runtime = Some(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
    );
    let runtime = fixture.runtime.as_ref().context("Runtime missing")?;
    fixture.server = Some(runtime.block_on(async {
        let (state, _) = AppState::new(Config {
            data_dir: owned.join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            max_project_files: Some(32),
            max_project_bytes: Some(64 * 1024),
            ..Default::default()
        })?;
        Ok::<_, anyhow::Error>(Engram::new(state))
    })?);
    let server = fixture.server.as_ref().context("Server missing")?;
    runtime.block_on(async {
        let indexed = text(server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":expected.name,"project_type":"general","wait":true
        }))?).await?)?;
        let project_id = indexed.split_once("project_id: ").and_then(|(_, rest)| rest.lines().next())
            .with_context(|| format!("Indexing returned no identity: {indexed}"))?.trim();
        ensure!(server.state.registry.active_source_generation(project_id)? > 0);
        let mut rows = Vec::new();
        for query in &expected.queries {
            let output = text(server.handle_search_memory(serde_json::from_value(json!({
                "project_id":project_id,"query":query.query,"semantic":false,"use_mmr":false,
                "fts_mode":"loose","search_scope":"code","max_results":expected.candidate_chunks,
                "output_json":true,"include_content":false
            }))?).await?)?;
            let reply: Value = serde_json::from_str(&output)?;
            let results = reply["results"].as_array().context("Search returned no typed results")?;
            let chunks = results.iter().map(|hit| {
                let path = hit["path"].as_str().context("Search hit missing source path")?.replace('\\', "/");
                ensure!(expected.source_files.contains(&path), "Unexpected source hit: {path}");
                Ok(path)
            }).collect::<Result<Vec<_>>>()?;
            let paths = distinct_paths(chunks.clone(), expected.k);
            let mut row = score(&paths, &query.relevant_paths, expected.k);
            row["query_id"] = json!(query.query_id); row["query"] = json!(query.query);
            row["candidate_chunk_paths"] = json!(chunks);
            println!("RETRIEVAL_QUERY {row}");
            rows.push(row);
        }
        let mean = |key: &str| rows.iter().map(|r| r[key].as_f64().unwrap_or(0.0)).sum::<f64>() / rows.len() as f64;
        let passed = passes(&rows, &expected);
        println!("RETRIEVAL_REPORT {}", json!({
            "corpus":expected.name,"backend":"fts_only","k":expected.k,
            "candidate_chunks":expected.candidate_chunks,"unit":"distinct source files in first-occurrence chunk rank order",
            "mean_precision_at_k":mean("precision_at_k"),"mean_reciprocal_rank":mean("reciprocal_rank"),
            "passed":passed,"queries":rows,
            "limits":"Authored lexical regression cases; not a semantic, held-out model, private application or productivity evaluation."
        }));
        let failed: Vec<_> = rows.iter().filter(|r| r["recall_at_k"].as_f64() != Some(1.0))
            .map(|r| r["query_id"].clone()).collect();
        ensure!(passed, "Retrieval gate failed: missing expected answers={failed:?}; see RETRIEVAL_QUERY/REPORT for ranking failures");
        Ok(())
    })
}

#[test]
fn gate_rejects_missing_answers_bad_ranks_and_duplicate_chunk_credit() -> Result<()> {
    let (_, expected) = oracle()?;
    let mut rows: Vec<_> = expected
        .queries
        .iter()
        .map(|q| score(&q.relevant_paths, &q.relevant_paths, expected.k))
        .collect();
    ensure!(passes(&rows, &expected));
    rows[0] = score(&[], &expected.queries[0].relevant_paths, expected.k);
    ensure!(
        !passes(&rows, &expected),
        "Other successful queries must not conceal a missing answer"
    );
    rows = expected
        .queries
        .iter()
        .map(|q| {
            let ranked = vec![
                "unrelated-1".into(),
                "unrelated-2".into(),
                "unrelated-3".into(),
                "unrelated-4".into(),
                q.relevant_paths[0].clone(),
            ];
            score(&ranked, &q.relevant_paths, expected.k)
        })
        .collect();
    ensure!(
        !passes(&rows, &expected),
        "All answers at rank five must fail the MRR floor"
    );
    let relevant = vec!["answer.vb".to_owned()];
    let paths = distinct_paths(
        vec![
            "noise.vb".into(),
            "noise.vb".into(),
            "answer.vb".into(),
            "answer.vb".into(),
        ],
        5,
    );
    let result = score(&paths, &relevant, 5);
    ensure!(result["precision_at_k"] == 0.2 && result["reciprocal_rank"] == 0.5);
    Ok(())
}
