//! A maintenance request queued behind a real update must collect using the
//! head that actually published, while preserving an already active reader.
use super::*;

#[test]
fn queued_gc_refreshes_the_published_head_after_a_background_update() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let engram = fixture_at(owned.path(), if cfg!(feature="vector") {"local"} else {"fts_only"});
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))?).await?;
            finish_owned_jobs(&engram).await;
            let previous = engram.state.registry.active_source_generation(PROJECT)?;
            let ps = engram.ensure_project_runtime(PROJECT).await?;
            let old_reader = ps.search.pin_source_generation(PROJECT,previous)?;
            #[cfg(feature="vector")]
            let old_vectors: std::collections::BTreeSet<_> = ps.search.vector_search(&query(previous,"new_marker"),&CancellationToken::new()).await?.into_iter().map(|h|h.pk).collect();
            std::fs::write(owned.path().join("repo/source.rs"),"pub fn after_wait_marker() -> u32 { 333 }\n")?;
            let pause = PublicationPause {
                project:PROJECT.into(),after_commit:false,fail_cleanup:false,
                entered:Arc::new(tokio::sync::Notify::new()),resume:Arc::new(tokio::sync::Notify::new()),
            };
            let registry_id = Arc::as_ptr(&engram.state.registry) as usize;
            BACKGROUND_PUBLICATION_PAUSES.lock().unwrap_or_else(|e|e.into_inner()).insert(registry_id,pause.clone());
            let release = ResumePublication {registry_id,resume:pause.resume.clone()};
            let submitted = engram.handle_update_project(serde_json::from_value(json!({
                "project_id":PROJECT,"wait":false,"max_commits":1,"index_antipatterns":false
            }))?).await;
            let reached = submitted.is_ok() && tokio::time::timeout(std::time::Duration::from_secs(30),pause.entered.notified()).await.is_ok();
            let (queued,gc) = {
                // This warmed current-thread fixture has an existing mutex
                // and no other lock-map writers. The handler's first await
                // acquires that mutex; its map read must be immediately ready.
                // Reassess this boundary if an earlier suspension is added.
                let map_read_ready = engram.state.project_update_locks.try_read().is_ok();
                let collecting = engram.handle_incremental_indexing_gc(serde_json::from_value(json!({
                    "project_id":PROJECT,"compact_vectors":false
                }))?);
                tokio::pin!(collecting);
                let first = futures::poll!(&mut collecting);
                let queued = map_read_ready && first.is_pending();
                // Release actual publication before any assertion or error
                // can unwind, and finish every owned background job below.
                drop(release);
                let response = match first {
                    std::task::Poll::Ready(response) => response,
                    std::task::Poll::Pending => tokio::time::timeout(std::time::Duration::from_secs(30),&mut collecting).await?,
                };
                (queued,response)
            };
            finish_owned_jobs(&engram).await;
            let current = engram.state.registry.active_source_generation(PROJECT)?;
            let fresh = ps.search.lexical_search(&query(current,"after_wait_marker"))?;
            let retained = ps.search.lexical_search(&query(previous,"new_marker"))?;
            #[cfg(feature="vector")]
            let vector_oracles = {
                let held: std::collections::BTreeSet<_> = ps.search.vector_search(&query(previous,"new_marker"),&CancellationToken::new()).await?.into_iter().map(|h|h.pk).collect();
                let live = ps.search.vector_search(&query(current,"after_wait_marker"),&CancellationToken::new()).await?;
                !old_vectors.is_empty() && held==old_vectors && !live.is_empty() && live.iter().all(|h|!old_vectors.contains(&h.pk))
            };
            #[cfg(not(feature="vector"))]
            let vector_oracles = true;
            drop(old_reader);drop(ps);drop(engram);
            anyhow::ensure!(reached && queued,"Fixture must queue public GC behind a real staged background update");
            anyhow::ensure!(current==previous+1,"Background update must publish the next generation");
            let response = gc?;
            let response = &response.content[0].as_text().unwrap().text;
            anyhow::ensure!(response.contains(&format!("target_gen={current}")),"Queued GC used a stale head: {response}");
            anyhow::ensure!(!fresh.is_empty() && !retained.is_empty() && vector_oracles,"Collection must preserve both current source and an already pinned old reader");
            println!("QUEUED_GC_BEHAVIOR previous={previous} collected_head={current} old_reader_preserved=true");
            Ok::<_,anyhow::Error>(())
        })
    };
    owned.close()?;
    outcome
}
