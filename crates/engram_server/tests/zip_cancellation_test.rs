//! Gate test: ENG-AUD-0003 — background zip-history jobs must be wired to
//! cancellation tokens so that cancel_job_internal can cooperatively stop them.
#![allow(clippy::unwrap_used)]

use engram_core::Config;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use std::io::Write;
use std::time::Duration;
use tempfile::tempdir;
use zip::write::SimpleFileOptions;

fn make_zip(path: &std::path::Path, files: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default();
    for (name, data) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

/// Verify that ingest_zip_history(wait=false) inserts the job's cancellation
/// token into state.cancellation_tokens immediately upon return, and that the
/// token is removed once the job reaches a terminal state.
#[tokio::test]
async fn test_zip_history_background_cancellation_token_is_wired() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let zip_dir = root.join("zips");
    std::fs::create_dir_all(&zip_dir).unwrap();

    // Two snapshots are the minimum to produce temporal edges.
    make_zip(
        &zip_dir.join("01_snap.zip"),
        &[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")],
    );
    make_zip(
        &zip_dir.join("02_snap.zip"),
        &[("a.rs", b"fn a_v2() {}"), ("b.rs", b"fn b() {}")],
    );

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("data"),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // Create an empty project so we have a valid project_id.
    let proj_dir = root.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: proj_dir.to_string_lossy().into(),
            project_name: "ZipCancelTest".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = projects[0].project_id.clone();

    // Launch background ingestion.
    let res = engram
        .ingest_zip_history(Parameters(engram_server::IngestZipHistoryRequest {
            project_id: project_id.clone(),
            directory: zip_dir.to_string_lossy().into(),
            wait: false,
        }))
        .await
        .unwrap();

    // Extract job_id from the response text.
    let text = &res.content[0].as_text().unwrap().text;
    let job_id = text
        .lines()
        .find_map(|l| l.strip_prefix("job_id: "))
        .expect("response should contain job_id field")
        .trim()
        .to_string();

    // The cancellation token must be present immediately after the call returns,
    // because it is inserted before the background task is spawned.
    {
        let tokens = state.cancellation_tokens.read().await;
        assert!(
            tokens.contains_key(&job_id),
            "cancellation_tokens must contain the job's token right after wait=false call"
        );
    }

    // Poll until the job reaches a terminal state (completed or failed).
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let job = state.registry.get_job(&job_id).unwrap();
        if let Some(j) = &job {
            if j.status == "completed" || j.status == "failed" {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("zip history background job did not reach terminal state within 30 s");
        }
    }

    // After completion the token must be cleaned up.
    {
        let tokens = state.cancellation_tokens.read().await;
        assert!(
            !tokens.contains_key(&job_id),
            "cancellation_tokens must NOT contain the job token after the job completes"
        );
    }

    // Verify the job reached a successful terminal state with correct metadata.
    let job = state.registry.get_job(&job_id).unwrap().unwrap();
    assert_eq!(job.status, "completed");
    assert_eq!(
        job.progress_pct, 100,
        "completed job should have progress_pct=100"
    );
}

/// Verify that after cancellation the job status is "cancelled" (or "failed"),
/// and that the cancellation token is removed from the tracking map.
#[tokio::test]
async fn test_zip_history_background_cancel_removes_token() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let zip_dir = root.join("zips");
    std::fs::create_dir_all(&zip_dir).unwrap();

    // Two snapshots minimum.
    make_zip(
        &zip_dir.join("01_snap.zip"),
        &[("x.rs", b"fn x() {}")],
    );
    make_zip(
        &zip_dir.join("02_snap.zip"),
        &[("x.rs", b"fn x_v2() {}")],
    );

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("data"),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    let proj_dir = root.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: proj_dir.to_string_lossy().into(),
            project_name: "ZipCancelTest2".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = projects[0].project_id.clone();

    let res = engram
        .ingest_zip_history(Parameters(engram_server::IngestZipHistoryRequest {
            project_id: project_id.clone(),
            directory: zip_dir.to_string_lossy().into(),
            wait: false,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    let job_id = text
        .lines()
        .find_map(|l| l.strip_prefix("job_id: "))
        .expect("response should contain job_id")
        .trim()
        .to_string();

    // Token must exist before cancellation.
    {
        let tokens = state.cancellation_tokens.read().await;
        assert!(tokens.contains_key(&job_id), "token must exist before cancel");
    }

    // Cancel via the service function (replicates what the cancel_job tool does).
    let cancelled = engram_server::services::job_service::cancel_job_internal(&state, &job_id).await;
    assert!(cancelled, "cancel_job_internal must return true when token exists");

    // Token must be removed by cancel_job_internal immediately.
    {
        let tokens = state.cancellation_tokens.read().await;
        assert!(
            !tokens.contains_key(&job_id),
            "token must be removed after cancellation"
        );
    }

    // Poll until the job record reflects a terminal status (cancelled or failed).
    // The tombstone write inside cancel_job_internal is async (spawn_blocking),
    // so we give it a short window to propagate.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(Some(j)) = state.registry.get_job(&job_id) {
            if j.status == "cancelled" || j.status == "failed" {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            let status = state
                .registry
                .get_job(&job_id)
                .ok()
                .flatten()
                .map(|j| j.status.clone())
                .unwrap_or_else(|| "<not found>".into());
            panic!(
                "job did not reach terminal status within 5 s after cancel; last status: {status}"
            );
        }
    }

    let job = state.registry.get_job(&job_id).unwrap().unwrap();
    assert!(
        job.status == "cancelled" || job.status == "failed",
        "job status must be 'cancelled' or 'failed' after cancel_job_internal; got '{}'",
        job.status
    );
}
