#![cfg(feature = "vector")]
#![allow(clippy::unwrap_used)]

use engram_core::Config;
use engram_server::{AppState, Engram, IndexProjectRequest, UpdateProjectRequest};
use rmcp::model::{CallToolResult, RawContent};

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(value) => Some(value.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn drain_jobs(state: &AppState) -> bool {
    let mut completed = true;
    loop {
        let jobs: Vec<_> = state
            .active_jobs
            .write()
            .await
            .drain()
            .map(|(_, job)| job)
            .collect();
        if jobs.is_empty() {
            return completed;
        }
        for job in jobs {
            completed &= job.await.is_ok();
        }
    }
}

async fn schema_failure_preserves_published_source(wait: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // TEMP belongs to the shared build checkout. This source-only fixture
    // must not discover or enqueue history from that parent repository.
    drop(git2::Repository::init(&project).unwrap());
    std::fs::write(project.join("a.rs"), "pub fn changed_marker() {}\n").unwrap();
    std::fs::write(project.join("b.rs"), "pub fn inherited_marker() {}\n").unwrap();
    let (state, _events) = AppState::new(Config {
        data_dir: temporary.path().join("data"),
        allowed_roots: vec![project.clone()],
        embedding_backend: "local".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    let indexed = engram
        .handle_index_project(IndexProjectRequest {
            directory: project.to_string_lossy().into_owned(),
            project_name: "vector-recovery".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: true,
        })
        .await;
    // Even wait=true may enqueue history. Settle its actual owners before
    // modifying the native table or releasing the fixture's databases.
    let initial_jobs_completed = drain_jobs(&state).await;
    let indexed = indexed.unwrap();
    let output = text(&indexed);
    let project_id = output
        .lines()
        .find_map(|line| {
            line.split_once("project_id: ")
                .map(|(_, value)| value.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("index did not return a project: {output}"));
    let old_head = state
        .registry
        .get_meta(&project_id, "active_generation")
        .unwrap()
        .unwrap();
    let old_generation = old_head.parse::<u64>().unwrap();
    let runtime = state.get_project_cached(&project_id).unwrap();
    let expected_paths = runtime
        .search
        .paths_in_generation(&project_id, Some("memory"), old_generation)
        .unwrap();
    assert_eq!(expected_paths.len(), 2);

    // Simulate an incompatible existing vector store. This fixture also has
    // history data, which a failed source update has no right to discard.
    let table_name = format!("project_{}", project_id.replace('-', "_"));
    let connection = engram_index::vector::connect(
        &state
            .cfg
            .data_dir
            .join("projects")
            .join(&project_id)
            .join("lancedb"),
    )
    .await
    .unwrap();
    connection.drop_table(&table_name, &[]).await.unwrap();
    {
        let (table, _) = engram_index::vector::open_or_create_table(&connection, &table_name, 4)
            .await
            .unwrap();
        let batch = engram_index::vector::create_record_batch(
            &project_id,
            "history",
            1,
            &["history-pk".into()],
            &["history-doc".into()],
            &["history-hash".into()],
            &[1],
            &["a.rs".into()],
            &["rust".into()],
            &[None],
            &[None],
            &[vec![0.25; 4]],
            4,
        )
        .unwrap();
        engram_index::vector::upsert_vectors(&table, vec![batch])
            .await
            .unwrap();
    }

    let result = engram
        .handle_update_project(UpdateProjectRequest {
            only_paths: None,
            instruction_maintenance: None,
            project_id: project_id.clone(),
            wait,
            max_commits: 1,
            index_antipatterns: false,
            reindex_paths: vec!["a.rs".into()],
        })
        .await;
    if wait {
        let error =
            result.expect_err("an incompatible vector store must not publish a partial update");
        assert!(error.message.contains("VEC1"), "{error}");
    } else {
        let output = text(&result.unwrap());
        let job_id = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("job_id: "))
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            let job = state.registry.get_job(job_id).unwrap().unwrap();
            let finished = !state.active_jobs.read().await.contains_key(job_id);
            if finished {
                assert_eq!(job.status, "failed", "{}", job.message);
                assert!(job.message.contains("VEC1"), "{}", job.message);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "update job did not finish"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    assert_eq!(
        state
            .registry
            .get_meta(&project_id, "active_generation")
            .unwrap()
            .as_deref(),
        Some(old_head.as_str())
    );
    assert!(
        state
            .registry
            .get_project(&project_id)
            .unwrap()
            .unwrap()
            .reindex_required_since_ms
            .is_some(),
        "the recovery requirement must survive a restart even without an event consumer"
    );
    assert_eq!(
        runtime
            .search
            .paths_in_generation(&project_id, Some("memory"), old_generation)
            .unwrap(),
        expected_paths
    );
    {
        let table = connection.open_table(&table_name).execute().await.unwrap();
        assert_eq!(
            table
                .count_rows(Some("namespace = 'history'".into()))
                .await
                .unwrap(),
            1,
            "a source-update failure must preserve other namespaces"
        );
    }
    let final_jobs_completed = drain_jobs(&state).await;
    drop(connection);
    drop(runtime);
    drop(engram);
    drop(state);
    temporary.close().unwrap();
    assert!(
        initial_jobs_completed && final_jobs_completed,
        "Owned background jobs must finish without panicking"
    );
}

#[tokio::test]
async fn synchronous_update_does_not_publish_or_destroy_history_on_vector_schema_failure() {
    schema_failure_preserves_published_source(true).await;
}

#[tokio::test]
async fn background_update_does_not_publish_or_destroy_history_on_vector_schema_failure() {
    schema_failure_preserves_published_source(false).await;
}
