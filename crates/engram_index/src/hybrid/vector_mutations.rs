//! Keep native writes and their admission alive through request cancellation.
use super::*;

#[cfg(test)]
pub(super) struct NativeWritePause {
    pub entered: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
    pub started: std::sync::atomic::AtomicBool,
}

impl HybridSearchEngine {
    pub(super) fn reserve_vector_batch(
        &self,
        project: &str,
        docs: &[IndexDoc],
    ) -> anyhow::Result<Option<AllocationGuard>> {
        let vector_bytes = (self.embedder.dimension() as u64)
            .checked_mul(12)
            .ok_or_else(|| anyhow::anyhow!("Vector batch size overflow"))?;
        let bytes = docs
            .iter()
            .try_fold(0u64, |total, doc| -> anyhow::Result<u64> {
                // Retained vectors, the flattened Arrow copy and native handoff;
                // string arrays/PKs and the provider's text serialization coexist.
                let strings = [
                    project,
                    doc.path.as_str(),
                    &doc.doc_id,
                    &doc.content_hash,
                    &doc.namespace,
                    &doc.language,
                    doc.author.as_deref().unwrap_or(""),
                ]
                .iter()
                .try_fold(0u64, |sum, value| sum.checked_add(value.len() as u64))
                .ok_or_else(|| anyhow::anyhow!("Vector metadata size overflow"))?;
                let row = strings
                    .checked_mul(3)
                    .and_then(|n| n.checked_add(vector_bytes))
                    .and_then(|n| n.checked_add((doc.content.len() as u64).checked_mul(2)?))
                    .and_then(|n| n.checked_add(256))
                    .ok_or_else(|| anyhow::anyhow!("Vector row size overflow"))?;
                total
                    .checked_add(row)
                    .ok_or_else(|| anyhow::anyhow!("Vector batch size overflow"))
            })?;
        self.memory_budget
            .as_ref()
            .map(|budget| {
                AllocationGuard::try_new(
                    budget,
                    bytes,
                    Subsystem::LanceDb,
                    "retained embedding and vector batch",
                )
            })
            .transpose()
    }

    pub(super) async fn run_native_vector_mutation<T, F>(&self, operation: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let permit = self.begin_operation()?;
        self.run_reserved_native_mutation(permit, operation).await
    }

    pub(super) async fn run_native_vector_mutation_with_commit<T, F>(
        &self,
        commit: &mut commit::Guard,
        operation: F,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        // All fallible admission precedes delegation and task ownership.
        let permit = self.begin_operation()?;
        let native = commit.delegate(self)?;
        let (result, native) = self
            .run_reserved_native_mutation(permit, async move {
                // Return authority even when the backend reports an ordinary
                // failure, so a caller may deliberately continue its batch.
                Ok((operation.await, native))
            })
            .await?;
        commit.restore(native)?;
        result
    }

    async fn run_reserved_native_mutation<T, F>(
        &self,
        permit: IndexOperationGuard,
        operation: F,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        #[cfg(test)]
        let pause = self.native_write_pause.clone();
        tokio::spawn(async move {
            let _permit = permit;
            #[cfg(test)]
            if let Some(pause) = pause {
                if !pause
                    .started
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    pause.entered.notify_one();
                    pause.release.notified().await;
                }
            }
            // Never race cancellation against this future. The requesting
            // future may disappear; native mutation admission ends with work.
            operation.await
        })
        .await
        .map_err(|e| anyhow::anyhow!("Native vector mutation did not complete: {e}"))?
    }

    pub(super) async fn delete_vector_rows(
        &self,
        commit: &mut commit::Guard,
        table: &lancedb::Table,
        filter: String,
    ) -> anyhow::Result<()> {
        let native_store = self.native_store.clone();
        let table_name = table.name().to_owned();
        self.run_native_vector_mutation_with_commit(commit, async move {
            let native = native_store.capture()?;
            crate::vector::delete_rows_if_present(&native.connection, &table_name, &filter).await
        })
        .await
    }
}
