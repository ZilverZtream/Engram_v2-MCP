//! One ordered payload mutation across lexical and vector stores.
use super::*;

/// Diagnostics travel with the actual lexical writer, including its Drop
/// finalizer. A payload commit outcome alone never establishes source-head
/// publication, which remains a separate Registry operation.
#[derive(Clone)]
pub(super) struct LexicalContext {
    pub directory: PathBuf,
    pub project: Option<Arc<str>>,
    pub generation_hint: Option<u64>,
    pub operation: &'static str,
}

impl LexicalContext {
    pub fn commit(&self, writer: &mut tantivy::IndexWriter<tantivy::TantivyDocument>) -> anyhow::Result<tantivy::Opstamp> {
        writer.commit().map_err(|error| {
            let detail = format!("lexical stage=commit operation={} project={} generation_hint={:?} index_directory={} commit=failed_ambiguous; source publication not established: {error}",
                self.operation, self.project.as_deref().unwrap_or("<unscoped>"), self.generation_hint, self.directory.display());
            tracing::error!(operation=self.operation, project_id=?self.project, generation_hint=?self.generation_hint,
                index_directory=%self.directory.display(), stage="commit", commit_outcome="failed_ambiguous", error=%error,
                "Lexical payload commit failed; authoritative source head must be inspected separately");
            anyhow::Error::new(error).context(detail)
        })
    }

    pub fn wait_merges(&self, writer: tantivy::IndexWriter<tantivy::TantivyDocument>, opstamp: tantivy::Opstamp) -> anyhow::Result<()> {
        writer.wait_merging_threads().map_err(|error| {
            let detail = format!("lexical stage=wait_merging_threads operation={} project={} generation_hint={:?} index_directory={} commit=succeeded opstamp={opstamp}; finalization failed, source publication not established: {error}",
                self.operation, self.project.as_deref().unwrap_or("<unscoped>"), self.generation_hint, self.directory.display());
            tracing::error!(operation=self.operation, project_id=?self.project, generation_hint=?self.generation_hint,
                index_directory=%self.directory.display(), stage="wait_merging_threads", commit_outcome="succeeded", opstamp, error=%error,
                "Lexical payload committed but merge finalization failed; source publication is separate");
            anyhow::Error::new(error).context(detail)
        })
    }

    pub fn finish(&self, mut writer: tantivy::IndexWriter<tantivy::TantivyDocument>) -> anyhow::Result<()> {
        let opstamp = self.commit(&mut writer)?;
        self.wait_merges(writer, opstamp)
    }
}

struct Lease {
    coordinator: Arc<mutation::Coordinator>,
    // Keep both exclusion mechanisms alive until all backend finalizers end.
    _lane: tokio::sync::OwnedMutexGuard<()>,
    _admission: IndexMutationGuard,
}

/// Unique authority for a composite write. The private Arc is a lifetime
/// fence, never an independently usable permission to mutate another batch.
pub(super) struct Guard {
    lease: Arc<Lease>,
    delegated: bool,
}

#[cfg(feature = "vector")]
pub(super) struct Native {
    lease: Arc<Lease>,
    // Last field: notify only after this token's actual lease reference drops.
    #[cfg(all(test, feature = "vector"))]
    _completed: Option<Completed>,
}

#[cfg(all(test, feature = "vector"))]
struct Completed(Arc<tokio::sync::Notify>);

#[cfg(all(test, feature = "vector"))]
impl Drop for Completed {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[cfg(all(test, feature = "vector"))]
pub(super) struct FinalizePause {
    pub entered: Arc<tokio::sync::Notify>,
    pub release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    pub started: std::sync::atomic::AtomicBool,
}

#[cfg(all(test, feature = "vector"))]
impl FinalizePause {
    pub(super) fn wait(&self) {
        if !self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            if let Ok(release) = self.release.lock() {
                let _ = release.recv_timeout(std::time::Duration::from_secs(15));
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.lease.coordinator.mark_faulted();
        }
    }
}

#[cfg(feature = "vector")]
impl Drop for Native {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.lease.coordinator.mark_faulted();
        }
    }
}

impl Guard {
    pub(super) fn ensure_active(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.delegated,
            "Write ownership is detached; finish or drop this writer"
        );
        Ok(())
    }

    pub(super) fn validate(&self, engine: &HybridSearchEngine) -> anyhow::Result<()> {
        self.ensure_active()?;
        anyhow::ensure!(
            Arc::ptr_eq(&self.lease.coordinator, &engine.snapshots.mutations),
            "Write ownership belongs to another index root"
        );
        engine
            .snapshots
            .mutations
            .validate_mutation(&self.lease._admission._permit)?;
        if let Some(project) = self.lease._admission.project.as_deref() {
            anyhow::ensure!(
                engine
                    .admission
                    .as_ref()
                    .and_then(|scope| scope.project.as_deref())
                    == Some(project),
                "Write ownership belongs to another project"
            );
        }
        Ok(())
    }

    #[cfg(feature = "vector")]
    pub(super) fn delegate(&mut self, engine: &HybridSearchEngine) -> anyhow::Result<Native> {
        self.validate(engine)?;
        self.delegated = true;
        Ok(Native {
            lease: self.lease.clone(),
            #[cfg(all(test, feature = "vector"))]
            _completed: engine.native_commit_completed.clone().map(Completed),
        })
    }

    #[cfg(feature = "vector")]
    pub(super) fn restore(&mut self, native: Native) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.delegated && Arc::ptr_eq(&self.lease, &native.lease),
            "Native completion does not own this write batch"
        );
        self.delegated = false;
        Ok(())
    }
}

impl HybridSearchEngine {
    pub(super) async fn acquire_commit(&self, cancel: &CancellationToken) -> anyhow::Result<Guard> {
        anyhow::ensure!(!cancel.is_cancelled(), "Write admission cancelled");
        let admission = self.begin_mutation()?;
        let pending = self.snapshots.mutations.commit_lane.clone().lock_owned();
        tokio::pin!(pending);
        let lane = match futures::poll!(&mut pending) {
            std::task::Poll::Ready(lane) => lane,
            std::task::Poll::Pending => {
                #[cfg(all(test, feature = "vector"))]
                if let Some(waiting) = &self.commit_waiting {
                    waiting.notify_one();
                }
                tokio::select! {
                    lane = &mut pending => lane,
                    _ = cancel.cancelled() => anyhow::bail!("Write admission cancelled"),
                }
            }
        };
        self.snapshots
            .mutations
            .validate_mutation(&admission._permit)?;
        Ok(Guard {
            lease: Arc::new(Lease {
                coordinator: self.snapshots.mutations.clone(),
                _lane: lane,
                _admission: admission,
            }),
            delegated: false,
        })
    }

    pub(super) fn try_commit(&self) -> anyhow::Result<Guard> {
        let admission = self.begin_mutation()?;
        let lane = self
            .snapshots
            .mutations
            .commit_lane
            .clone()
            .try_lock_owned()
            .map_err(|_| mutation::CoordinationError::Busy)?;
        self.snapshots
            .mutations
            .validate_mutation(&admission._permit)?;
        Ok(Guard {
            lease: Arc::new(Lease {
                coordinator: self.snapshots.mutations.clone(),
                _lane: lane,
                _admission: admission,
            }),
            delegated: false,
        })
    }
}
