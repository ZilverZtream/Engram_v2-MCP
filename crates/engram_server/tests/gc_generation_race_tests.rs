#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-1 (docs/audits/09): OciusX's searchable
//! corpus collapsed to 56 VB chunks because the GC's `KeepLatestOnly` purge
//! deletes every generation that is NOT the published one — including the
//! generation an incremental update is still building by copy-forward — and
//! neither the watcher's `update_project_impl` nor `update_project(wait=true)`
//! registers itself in `active_indexing_count`, so the JOB1 guard never
//! fires. The incomplete generation is then published.
//!
//! Three reproductions, from the engine semantics up to the auditor's race:
//! 1. purge(active = N) must never delete generation N+1 (in construction);
//! 2. update_project_impl must hold the active-indexing slot while it runs;
//! 3. a GC sweep racing an incremental update must not shrink the corpus the
//!    update publishes.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

const CODE_NS: &str = "memory"; // where the indexer stores code chunks (KeepLatestOnly)

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
    // The incremental update walks git for changed files: the fixture is a repo.
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
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "GcRaceFixture".into(),
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

fn code_docs(state: &AppState, pid: &str) -> usize {
    let engine = state.get_project_cached(pid).unwrap().search;
    engine
        .count_docs_by_namespace(pid)
        .unwrap()
        .get(CODE_NS)
        .copied()
        .unwrap_or(0)
}

/// 1. Engine semantics: a purge baselined on the PUBLISHED generation must
///    leave the NEWER, in-construction generation untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_never_deletes_the_generation_being_built() {
    let (_tmp, state, _engram, pid) = build(6).await;
    let published = active_gen(&state, &pid);
    let before = code_docs(&state, &pid);
    assert!(before >= 6, "fixture must index code chunks, got {before}");

    // Copy-forward in progress: two chunks already written at generation N+1.
    let engine = state.get_project_cached(&pid).unwrap().search;
    let docs: Vec<engram_index::IndexDoc> = (0..2)
        .map(|i| engram_index::IndexDoc {
            generation: published + 1,
            chunk_id: 9000 + i,
            doc_id: format!("next-gen:{i}"),
            content_hash: format!("nh{i}"),
            path: RelPath::new(&format!("Site/App_Code/mod{i:02}.vb")),
            content: format!("Public Class mod{i:02} copied forward"),
            language: "vbnet".into(),
            namespace: CODE_NS.into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 3,
        })
        .collect();
    engine
        .index_docs(&pid, &docs, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(code_docs(&state, &pid), before + 2);

    // The GC runs while generation N+1 is still being built.
    engine.purge_old_generations(&pid, published).await.unwrap();

    assert_eq!(
        code_docs(&state, &pid),
        before + 2,
        "a purge baselined on the published generation {published} must not touch generation {} (in construction)",
        published + 1
    );

    // Once N+1 is published, the OLD generation goes — and only it.
    engine
        .purge_old_generations(&pid, published + 1)
        .await
        .unwrap();
    assert_eq!(
        code_docs(&state, &pid),
        2,
        "only the published generation survives"
    );
}

/// 2. The incremental update path holds the active-indexing slot for its
///    whole duration, so the JOB1/JOB3 guards in the GC actually see it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_project_impl_holds_the_active_indexing_slot() {
    let (_tmp, state, engram, pid) = build(12).await;
    assert_eq!(state.active_indexing_count.load(Ordering::SeqCst), 0);
    let new_gen = active_gen(&state, &pid) + 1;

    let sampler_state = state.clone();
    let sampled_max = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sampled = sampled_max.clone();
    let stop = CancellationToken::new();
    let stop2 = stop.clone();
    let sampler = tokio::spawn(async move {
        while !stop2.is_cancelled() {
            let v = sampler_state.active_indexing_count.load(Ordering::SeqCst);
            sampled.fetch_max(v, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    });

    engram
        .update_project_impl(&pid, new_gen, 0, false, &CancellationToken::new())
        .await
        .unwrap();
    stop.cancel();
    sampler.await.unwrap();

    assert!(
        sampled_max.load(Ordering::SeqCst) >= 1,
        "update_project_impl must register in active_indexing_count while it runs (sampled max {})",
        sampled_max.load(Ordering::SeqCst)
    );
    assert_eq!(
        state.active_indexing_count.load(Ordering::SeqCst),
        0,
        "the slot is released when the update ends"
    );
}

/// 3. The auditor's race: a GC sweep runs while an incremental update is
///    copying unchanged documents forward. The generation the update publishes
///    must still hold the whole corpus.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_racing_an_incremental_update_never_publishes_an_incomplete_generation() {
    let (_tmp, state, engram, pid) = build(40).await;
    let before = code_docs(&state, &pid);
    let new_gen = active_gen(&state, &pid) + 1;

    let gc_state = state.clone();
    let gc_pid = pid.clone();
    let stop = CancellationToken::new();
    let stop2 = stop.clone();
    let gc = tokio::spawn(async move {
        let mut sweeps = 0usize;
        while !stop2.is_cancelled() {
            let _ = engram_server::actors::gc::purge_project_old_gens(&gc_state, &gc_pid).await;
            sweeps += 1;
            tokio::task::yield_now().await;
        }
        sweeps
    });

    engram
        .update_project_impl(&pid, new_gen, 0, false, &CancellationToken::new())
        .await
        .unwrap();
    stop.cancel();
    let sweeps = gc.await.unwrap();
    assert!(
        sweeps > 0,
        "the GC must have raced the update at least once"
    );

    assert_eq!(
        active_gen(&state, &pid),
        new_gen,
        "the update published its generation"
    );
    // Settle: one more purge after publish must leave exactly the new generation.
    let engine = state.get_project_cached(&pid).unwrap().search;
    engine.purge_old_generations(&pid, new_gen).await.unwrap();
    let after = code_docs(&state, &pid);
    assert_eq!(
        after, before,
        "generation {new_gen} was published INCOMPLETE: {after} code chunks, {before} before the update ({sweeps} GC sweeps raced it)"
    );
}
