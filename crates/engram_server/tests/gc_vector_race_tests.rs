#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10) P0-1 — "the GC race is only half
//! fixed": the Tantivy purge keeps a newer, in-construction generation, but
//! the LanceDB purge still deletes every generation `!= active`, the GC
//! guards itself with an atomic counter instead of the per-project update
//! lock (check-then-act), the round-1 race test counted Tantivy documents
//! only, and `update_project_impl` swallows a failed post-publication purge
//! with `.ok()`. Three reproductions:
//! 1. LanceDB rows written at generation N+1 must survive `purge(N)`;
//! 2. the GC must yield to a held project update lock (mutual exclusion);
//! 3. the post-publication purge outcome is recorded durably and the GC
//!    retries a pending purge.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_server::actors::gc::{GcOutcome, purge_project_old_gens};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use tokio_util::sync::CancellationToken;

const CODE_NS: &str = "memory";

async fn build(files: usize) -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    for i in 0..files {
        std::fs::write(
            root.join(format!("Site/App_Code/mod{i:02}.vb")),
            format!(
                "Public Class mod{i:02}\n    Public Function GetByID{i}(id As Integer) As String\n        Return \"redovisningskategori {i}\"\n    End Function\nEnd Class\n"
            ),
        )
        .unwrap();
    }
    for args in [
        vec!["init", "-q"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    ] {
        let st = std::process::Command::new("git")
            .args(&args)
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
        max_project_bytes: Some(4 * 1024 * 1024),
        // The built-in projection embedder: real LanceDB rows, no network.
        embedding_backend: "local".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "GcVectorRace".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

fn active_gen(state: &AppState, pid: &str) -> u64 {
    state
        .registry
        .get_meta(pid, "active_generation")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap()
}

fn next_gen_docs(published: u64, n: usize) -> Vec<engram_index::IndexDoc> {
    (0..n)
        .map(|i| engram_index::IndexDoc {
            generation: published + 1,
            chunk_id: 9000 + i as u64,
            doc_id: format!("next-gen:{i}"),
            content_hash: format!("nh{i}"),
            path: RelPath::new(&format!("Site/App_Code/mod{i:02}.vb")),
            content: format!(
                "Public Class mod{i:02} copied forward at generation {}",
                published + 1
            ),
            language: "vbnet".into(),
            namespace: CODE_NS.into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 3,
        })
        .collect()
}

/// 1. The LanceDB assertion the round-1 test lacked: both N and the in-flight
///    N+1 vector rows survive purge(N); only N goes once N+1 is published.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_keeps_the_in_flight_generations_vector_rows() {
    let (_tmp, state, _engram, pid) = build(6).await;
    let published = active_gen(&state, &pid);
    let engine = state.get_project_cached(&pid).unwrap().search;
    let rows_n = engine
        .count_vectors_in_generation(&pid, published)
        .await
        .unwrap();
    assert!(
        rows_n >= 6,
        "the fixture must have vector rows at generation {published}, got {rows_n}"
    );

    engine
        .index_docs(
            &pid,
            &next_gen_docs(published, 3),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let rows_next = engine
        .count_vectors_in_generation(&pid, published + 1)
        .await
        .unwrap();
    assert_eq!(
        rows_next,
        3,
        "three vector rows written at generation {}",
        published + 1
    );

    // The GC runs while N+1 is still being built.
    engine.purge_old_generations(&pid, published).await.unwrap();
    assert_eq!(
        engine
            .count_vectors_in_generation(&pid, published + 1)
            .await
            .unwrap(),
        3,
        "purge({published}) must not delete the in-flight generation {}'s VECTOR rows",
        published + 1
    );
    assert_eq!(
        engine
            .count_vectors_in_generation(&pid, published)
            .await
            .unwrap(),
        rows_n,
        "purge({published}) keeps the published generation's vector rows"
    );

    // Once N+1 is published, the old generation goes — and only it.
    engine
        .purge_old_generations(&pid, published + 1)
        .await
        .unwrap();
    assert_eq!(
        engine
            .count_vectors_in_generation(&pid, published)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        engine
            .count_vectors_in_generation(&pid, published + 1)
            .await
            .unwrap(),
        3
    );
}

/// 2. Mutual exclusion: while a project's update lock is held (an update is
///    building N+1 — the check-then-act window the counter guard leaves open),
///    the GC must yield instead of purging.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gc_yields_to_a_held_project_update_lock() {
    let (_tmp, state, _engram, pid) = build(4).await;
    let guard = state.acquire_project_update_lock(&pid).await;
    let outcome = purge_project_old_gens(&state, &pid).await.unwrap();
    assert_eq!(
        outcome,
        GcOutcome::SkippedUpdateInFlight,
        "the GC must not purge while the project's update lock is held"
    );
    drop(guard);
    let outcome = purge_project_old_gens(&state, &pid).await.unwrap();
    assert_eq!(
        outcome,
        GcOutcome::Purged,
        "with the lock free the GC purges"
    );
}

/// 3. The post-publication purge is a recorded step: `update_project_impl`
///    reports it, the registry carries `purge_pending` only while a purge is
///    owed, and the GC retries and clears a pending purge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_post_publish_purge_outcome_is_recorded_and_retried() {
    let (_tmp, state, engram, pid) = build(4).await;
    let new_gen = active_gen(&state, &pid) + 1;
    let report = engram
        .update_project_impl(&pid, new_gen, 0, false, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        report.contains("purge: ok"),
        "the update reports its post-publication purge outcome: {report}"
    );
    let pending = state
        .registry
        .get_meta(&pid, "purge_pending")
        .unwrap()
        .unwrap_or_default();
    assert!(
        pending.is_empty(),
        "no purge is owed after a successful update, got {pending:?}"
    );

    // A purge left owed (a failed post-publication purge) is retried by the GC.
    state
        .registry
        .set_meta(&pid, "purge_pending", &new_gen.to_string())
        .unwrap();
    let outcome = purge_project_old_gens(&state, &pid).await.unwrap();
    assert_eq!(outcome, GcOutcome::Purged);
    let pending = state
        .registry
        .get_meta(&pid, "purge_pending")
        .unwrap()
        .unwrap_or_default();
    assert!(
        pending.is_empty(),
        "the GC clears purge_pending after purging, got {pending:?}"
    );
}
