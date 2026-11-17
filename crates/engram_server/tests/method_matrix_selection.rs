//! Exact requested methods retain their QA evidence independently of file-wide windows.
use anyhow::{Result, ensure};
use engram_core::{Config, ContentHash, DocIdStr, RelPath};
use engram_index::IndexDoc;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::{future::Future, path::Path};
use tokio_util::sync::CancellationToken;

fn run<F, Fut>(check: F) -> Result<()>
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("mq-").tempdir_in(parent)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(check(owned.path().to_owned()));
    drop(runtime);
    match (result, owned.close()) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Fixture cleanup also failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

async fn project(owned: &Path, source: &str) -> Result<(Engram, String)> {
    let root = owned.join("r");
    std::fs::create_dir(&root)?;
    git2::Repository::init(&root)?;
    std::fs::write(root.join("Rules.vb"), source)?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let engram = Engram::new(state);
    let indexed = engram.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"Owned method QA selection","project_type":"general","wait":true
    }))?).await?;
    ensure!(
        indexed.is_error != Some(true),
        "Indexing failed: {indexed:?}"
    );
    let text = &indexed.content[0]
        .as_text()
        .ok_or_else(|| anyhow::anyhow!("Missing index result"))?
        .text;
    let pid = text
        .split("project_id: ")
        .nth(1)
        .and_then(|tail| tail.lines().next())
        .ok_or_else(|| anyhow::anyhow!("Missing project identity: {text}"))?
        .trim()
        .to_owned();
    Ok((engram, pid))
}

fn analysis(key: &str, name: &str, body: &str, chunk_id: u64) -> IndexDoc {
    let path = format!("__business_logic/Rules.vb/{key}.md");
    let hash = ContentHash::compute(path.as_bytes());
    let content = format!(
        "# Rules.{name}\n**Analysis method hash**: `{}`\n## Business Rules\n- Proposed return case for {key}.\n_Source: Rules.vb_\n",
        ContentHash::compute(body.as_bytes()).0,
    );
    IndexDoc {
        generation: 0,
        chunk_id,
        path: RelPath::new(&path),
        language: "markdown".into(),
        content_hash: hash.0.clone(),
        content,
        namespace: "business_logic".into(),
        author: None,
        timestamp: None,
        start_line: 0,
        end_line: 0,
        doc_id: DocIdStr::compute(&path, 0, 0, &hash).0,
    }
}

async fn seed(engram: &Engram, pid: &str, docs: &[IndexDoc]) -> Result<()> {
    let search = {
        let project = engram
            .state
            .projects
            .get(pid)
            .ok_or_else(|| anyhow::anyhow!("Missing indexed runtime"))?;
        project.search.clone()
    };
    search
        .index_docs(pid, docs, &CancellationToken::new())
        .await?;
    Ok(())
}

async fn matrix(engram: &Engram, pid: &str, line: u32) -> Result<String> {
    let result = engram
        .handle_derive_test_matrix(serde_json::from_value(json!({
            "project_id":pid,"files":["Rules.vb"],
            "methods":[{"file_path":"Rules.vb","line_start":line}],
            "max_source_cases":1,"max_coverage_notes":100
        }))?)
        .await?;
    ensure!(result.is_error != Some(true), "Matrix failed: {result:?}");
    Ok(result.content[0]
        .as_text()
        .ok_or_else(|| anyhow::anyhow!("Missing matrix"))?
        .text
        .clone())
}

#[test]
fn requested_current_analysis_survives_the_file_wide_window_and_is_not_duplicated() -> Result<()> {
    run(|owned| async move {
        let mut source = "Public Class Rules\n".to_owned();
        let mut docs = Vec::new();
        for index in 0..201 {
            let name = format!("Rule{index:03}");
            let body = format!("Public Function {name}() As Integer\nReturn {index}\nEnd Function");
            source.push_str(&body);
            source.push('\n');
            docs.push(analysis(&name, &name, &body, 10_000 + index));
        }
        source.push_str("End Class\n");
        let (engram, pid) = project(&owned, &source).await?;
        seed(&engram, &pid, &docs).await?;
        let searched = engram
            .handle_search_memory(serde_json::from_value(json!({
                "project_id":pid,"namespace":"business_logic","query":"*","fts_mode":"regex",
                "max_results":200,"use_mmr":false,"output_json":true
            }))?)
            .await?;
        let result: Value = serde_json::from_str(&searched.content[0].as_text().unwrap().text)?;
        let window = result["results"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing search results: {result}"))?;
        ensure!(
            window.len() == 200,
            "Fixture must reach the public window: {result}"
        );
        let (index, omitted) = docs
            .iter()
            .enumerate()
            .find(|(_, doc)| {
                !window
                    .iter()
                    .any(|hit| hit["doc_id"].as_str() == Some(doc.doc_id.as_str()))
            })
            .ok_or_else(|| anyhow::anyhow!("No stored analysis outside window"))?;
        let text = matrix(&engram, &pid, 2 + index as u32 * 3).await?;
        ensure!(
            text.contains(&omitted.doc_id) && text.contains("Expected outcome (inferred):"),
            "Requested analysis disappeared behind unrelated records: {text}"
        );
        ensure!(
            text.matches("### Matrix evidence D").count() == 1,
            "Wrong method scope: {text}"
        );
        ensure!(
            text.contains("canonical method") && text.contains("unexamined"),
            "Bounded legacy fallback must remain disclosed: {text}"
        );
        let included = docs
            .iter()
            .enumerate()
            .find(|(_, doc)| {
                window
                    .iter()
                    .any(|hit| hit["doc_id"].as_str() == Some(doc.doc_id.as_str()))
            })
            .unwrap()
            .0;
        let text = matrix(&engram, &pid, 2 + included as u32 * 3).await?;
        ensure!(
            text.matches("### Matrix evidence D").count() == 1
                && text.matches("Expected outcome (inferred):").count() == 1,
            "Direct and fallback lookup duplicated the same analysis: {text}"
        );
        Ok(())
    })
}

#[test]
fn exact_overload_selection_preserves_current_body_checks() -> Result<()> {
    run(|owned| async move {
        let first = "Public Function Pick(id As Integer) As Integer\nReturn id\nEnd Function";
        let second = "Public Function Pick(id As String) As String\nReturn id\nEnd Function";
        let source = format!("Public Class Rules\n{first}\n{second}\nEnd Class\n");
        let (engram, pid) = project(&owned, &source).await?;
        let docs = [
            analysis("Pick__L2", "Pick", first, 10_001),
            analysis("Pick__L5", "Pick", second, 10_002),
        ];
        seed(&engram, &pid, &docs).await?;
        let text = matrix(&engram, &pid, 5).await?;
        ensure!(
            text.contains(&docs[1].doc_id) && !text.contains(&docs[0].doc_id),
            "Wrong overload selected: {text}"
        );
        ensure!(
            text.matches("### Matrix evidence D").count() == 1,
            "Duplicated overload: {text}"
        );
        std::fs::write(
            owned.join("r/Rules.vb"),
            source.replace(
                second,
                "Public Function Pick(id As String) As String\nReturn id.Trim()\nEnd Function",
            ),
        )?;
        let text = matrix(&engram, &pid, 5).await?;
        ensure!(
            !text.contains("Expected outcome (inferred):") && text.contains("STALE:"),
            "Direct lookup admitted stale analysis: {text}"
        );
        Ok(())
    })
}
