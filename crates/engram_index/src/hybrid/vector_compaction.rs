//! Explicit maintenance combines small files without pruning reader versions.
use super::*;

impl HybridSearchEngine {
    /// Returns native fragment counts (removed, added), or no available native
    /// table. Compaction preserves row identities and old checked-out readers;
    /// it does not delete old versions or promise immediate disk reclamation.
    pub async fn compact_vector_fragments(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<(usize, usize)>> {
        #[cfg(not(feature = "vector"))]
        {
            let _ = project_id;
            Ok(None)
        }
        #[cfg(feature = "vector")]
        {
            let mutation = self.begin_project_mutation(project_id)?;
            let scoped = self.with_mutation(&mutation)?;
            let mut commit = scoped.acquire_commit(&CancellationToken::new()).await?;
            let epoch = scoped.pin_vector_epoch(project_id)?;
            scoped.ensure_managed_vector_table_present(&epoch)?;
            if !scoped
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&epoch.table)
            {
                return Ok(None);
            }
            let table = scoped.open_vector_epoch_table(&epoch).await?;
            scoped
                .run_native_vector_mutation_with_commit(&mut commit, async move {
                    // Request cancellation must not release the epoch or native
                    // mutation admission while a compaction is still publishing.
                    let _epoch = epoch;
                    crate::vector::prepare_managed_write(&table).await?;
                    let stats = table
                        .optimize(lancedb::table::OptimizeAction::Compact {
                            options: lancedb::table::CompactionOptions {
                                num_threads: Some(1),
                                batch_size: Some(1024),
                                ..Default::default()
                            },
                            remap_options: None,
                        })
                        .await?;
                    let counts = stats.compaction.unwrap_or_default();
                    Ok(Some((counts.fragments_removed, counts.fragments_added)))
                })
                .await
        }
    }
}
