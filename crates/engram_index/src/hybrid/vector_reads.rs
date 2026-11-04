//! A caller's timeout never releases a native reader that is still working.
use super::*;

#[cfg(test)]
pub(super) struct NativeReadPause {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub completed: tokio::sync::Notify,
    pub started: std::sync::atomic::AtomicBool,
}

struct Completion {
    routes: Arc<vector_epochs::Routes>,
    _epoch: Arc<vector_epochs::Epoch>,
    proven: bool,
}

impl Drop for Completion {
    fn drop(&mut self) {
        if !self.proven {
            self.routes.mark_read_unproven();
        }
    }
}

impl HybridSearchEngine {
    pub(super) async fn run_native_vector_query<T, F>(
        &self,
        epoch: Arc<vector_epochs::Epoch>,
        cancel: &CancellationToken,
        operation: F,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Native read cancelled before admission"
        );
        let slot = self
            .vector_routes
            .read_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("Native read capacity is busy; retry the operation"))?;
        let mut completion = Completion {
            routes: self.vector_routes.clone(),
            _epoch: epoch,
            proven: false,
        };
        #[cfg(test)]
        let pause = self.native_read_pause.clone();
        let worker = tokio::spawn(async move {
            let _slot = slot;
            #[cfg(test)]
            if let Some(pause) = &pause {
                if !pause
                    .started
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    pause.entered.notify_one();
                    pause.release.notified().await;
                }
            }
            // All native open/execute/count/stream work lives in this owned
            // future. It consumes bounded output through natural EOF. Neither
            // the request's cancellation nor its timeout aborts this task.
            let result = operation.await;
            completion.proven = result.is_ok();
            drop(completion);
            drop(_slot);
            #[cfg(test)]
            if let Some(pause) = pause {
                pause.completed.notify_one();
            }
            result
        });
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("Native read wait cancelled; backend reader retains its lease until completion"),
            result = worker => result.map_err(|error| anyhow::anyhow!("Native read worker failed: {error}"))?,
        }
    }
}
