//! Diagnostics follow the resource owner's lifetime rather than retaining it.
use super::*;

#[test]
fn observed_native_session_is_released_after_the_last_engine_owner_and_runtime_close()
-> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let weak = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let cfg = engram_core::Config {
                embedding_backend: "fts_only".into(),
                ..Default::default()
            };
            let engine = HybridSearchEngine::new(
                owned.path().join("fts"),
                owned.path().join("vectors"),
                &cfg,
            )
            .await?;
            let weak = Arc::downgrade(&engine.native_store.capture()?.session);
            let second_owner = engine.clone();
            for _ in 0..3 {
                let observed = engine.retention_observation("project");
                anyhow::ensure!(observed["native_cache"]["status"] == "compiled");
                anyhow::ensure!(observed["source_catalog"]["catalog_cached_views"] == 0);
            }
            drop(engine);
            anyhow::ensure!(
                weak.strong_count() > 0,
                "A live engine clone still owns the session"
            );
            drop(second_owner);
            Ok::<_, anyhow::Error>(weak)
        })?
    };
    anyhow::ensure!(
        weak.strong_count() == 0,
        "Observation must not create a longer-lived native session owner"
    );
    owned.close()?;
    Ok(())
}
