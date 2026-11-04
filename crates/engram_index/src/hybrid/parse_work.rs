//! Parse admission follows blocking work and unconsumed results, not its waiter.
//! Retained aggregate statistics and cross-iteration document batches need
//! separate output accounting; this reservation keeps its existing batch scope.
use super::*;

/// Extraction has not mutated native storage. A bad parser input is a failed
/// source update, not a broken storage-coordination invariant. Keep this catch
/// inside the work owner's scope and outside all native writes/publication.
pub(super) fn contain_extraction_panic<T>(
    path: &Path,
    extract: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(extract)).map_err(|_| {
        anyhow::anyhow!(
            "Source extraction panicked for {}; update was not completed",
            path.display()
        )
    })?
}

pub(super) struct ParsedBatch {
    pub docs: Vec<IndexDoc>,
    pub stats: IngestStats,
    // Output fields must be disposed before their admission and reservation.
    pub _work: ParseWork,
}

pub(super) struct ParseWork {
    pub reservation: Option<AllocationGuard>,
    pub operation: Option<IndexOperationGuard>,
    pub admission: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    pub vb_compilation: Option<Arc<crate::vb_extractor::VbCompilationLease>>,
    #[cfg(test)]
    pub pause: Option<Arc<ParsePause>>,
}

impl Drop for ParseWork {
    fn drop(&mut self) {
        drop(self.reservation.take());
        drop(self.operation.take());
        drop(self.vb_compilation.take());
        drop(self.admission.take());
        #[cfg(test)]
        if let Some(pause) = &self.pause {
            pause.disposed.notify_one();
        }
    }
}

impl HybridSearchEngine {
    /// Derive a temporary indexing view using the server's shared parse limit.
    /// Acquiring capacity happens in index_files, where it can follow workers.
    pub fn with_parse_limiter(&self, limiter: Arc<tokio::sync::Semaphore>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.parse_limiter
                .as_ref()
                .is_none_or(|current| Arc::ptr_eq(current, &limiter)),
            "Cannot replace an existing parse limiter"
        );
        let mut scoped = self.clone();
        scoped.parse_limiter = Some(limiter);
        Ok(scoped)
    }

    pub(super) async fn acquire_parse_permit(
        &self,
    ) -> anyhow::Result<Option<Arc<tokio::sync::OwnedSemaphorePermit>>> {
        use std::future::Future;
        let Some(limiter) = &self.parse_limiter else {
            return Ok(None);
        };
        let acquire = limiter.clone().acquire_owned();
        tokio::pin!(acquire);
        let permit = std::future::poll_fn(|cx| {
            let polled = acquire.as_mut().poll(cx);
            #[cfg(test)]
            if let Some(probe) = &self.parse_admission_probe {
                probe.state.store(
                    if polled.is_pending() { 1 } else { 2 },
                    std::sync::atomic::Ordering::SeqCst,
                );
                probe.changed.notify_one();
            }
            polled
        })
        .await
        .map_err(|error| anyhow::anyhow!("Parse semaphore closed: {error}"))?;
        Ok(Some(Arc::new(permit)))
    }
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct AdmissionProbe {
    pub state: std::sync::atomic::AtomicU8,
    pub changed: tokio::sync::Notify,
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct ParsePause {
    pub entered: tokio::sync::Notify,
    pub disposed: tokio::sync::Notify,
    started: std::sync::atomic::AtomicBool,
    panic_once: std::sync::atomic::AtomicBool,
    released: std::sync::Mutex<bool>,
    resume: std::sync::Condvar,
}

#[cfg(test)]
impl ParsePause {
    pub fn wait(&self) {
        if self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
        while !*released {
            released = self
                .resume
                .wait(released)
                .unwrap_or_else(|p| p.into_inner());
        }
        drop(released);
        if self
            .panic_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("controlled source extractor panic");
        }
    }

    pub fn release(&self) {
        *self.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.resume.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    struct ResumeOnDrop(Arc<ParsePause>);
    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    async fn engine(root: &Path, budget: Arc<MemoryBudget>) -> HybridSearchEngine {
        HybridSearchEngine::new_with_budget(
            root.join("tantivy"),
            root.join("lance"),
            &engram_core::Config {
                embedding_backend: "fts_only".into(),
                ..Default::default()
            },
            Some(budget),
        )
        .await
        .unwrap()
    }

    async fn index_one(engine: HybridSearchEngine, root: PathBuf) -> anyhow::Result<IngestStats> {
        engine
            .index_files(
                "project",
                "memory",
                1,
                &root,
                vec![root.join("source.rs")],
                1000,
                &CancellationToken::new(),
                |_, _| {},
            )
            .await
    }

    #[test]
    #[ignore = "requires v4 ENGRAM_VB_SIDECAR_PATH and serialized idle global parser ownership"]
    fn real_vb_cancelled_batch_keeps_compilation_owned_until_workers_finish_then_other_project_proceeds() -> anyhow::Result<()> {
        use anyhow::ensure;
        crate::vb_extractor::shutdown_idle_parser()?;
        struct ParserOwner;
        impl Drop for ParserOwner {
            fn drop(&mut self) {
                if let Err(error) = crate::vb_extractor::shutdown_idle_parser() {
                    eprintln!("Owned VB batch parser cleanup failed: {error:#}");
                }
            }
        }
        let owned = tempfile::tempdir()?;
        let owner = ParserOwner;
        let retained_budget = Arc::new(MemoryBudget::new(16 * 1024 * 1024));
        let result = (|| -> anyhow::Result<()> {
            ensure!(crate::vb_extractor::current_parser_context()?.roslyn.is_some(), "Actual v4 companion required");
            let root_a = owned.path().join("project A with spaces");
            let root_b = owned.path().join("project B with spaces");
            std::fs::create_dir(&root_a)?;
            std::fs::create_dir(&root_b)?;
            let source_a = "Public Class OnlyProjectA\nEnd Class\n";
            let source_b = "Public Class OnlyProjectB\nEnd Class\n";
            std::fs::write(root_a.join("source.vb"), source_a)?;
            std::fs::write(root_b.join("source.vb"), source_b)?;
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
            let tested = runtime.block_on(async {
                let budget = Arc::new(MemoryBudget::new(16 * 1024 * 1024));
                let first_cache_budget = budget.clone();
                // Independent engine/parse limiters: only the actual singleton
                // compilation authority can keep project B from overtaking A.
                let a = engine(&owned.path().join("store_a"), budget.clone()).await;
                let unscoped = a.index_files("project_a", "memory", 1, &root_a,
                    vec![root_a.join("source.vb")], 1000, &CancellationToken::new(), |_, _| {}).await;
                ensure!(unscoped.is_err() && a.count_docs("project_a")? == 0,
                    "Standalone parser code authority cannot certify inherited project bindings");
                let mut a = a.with_vb_source_inventory(Arc::new(vec![root_a.join("source.vb")]));
                let prepared = Arc::new(ParsePause::default());
                prepared.release();
                a.parse_preparation_pause = Some(prepared.clone());
                // A real admitted source must fail before preparation/storage
                // while concurrent work owns the remaining metadata budget.
                let baseline = budget.used();
                let pressure = AllocationGuard::try_new(&budget, budget.budget() - baseline - 256,
                    Subsystem::Misc, "owned concurrent parse pressure")?;
                let refused = a.index_files("budget_retry", "memory", 1, &root_a,
                    vec![root_a.join("source.vb")], 1000, &CancellationToken::new(), |_, _| {}).await;
                ensure!(refused.as_ref().is_err_and(|error| error.to_string().contains("VB compilation inventory/preparation"))
                    && !prepared.started.load(Ordering::SeqCst) && a.count_docs("budget_retry")? == 0
                    && budget.breakdown().parse_buffer == 0,
                    "Insufficient inventory capacity must refuse before actual child preparation or source writes");
                drop(pressure);
                let retried = a.index_files("budget_retry", "memory", 1, &root_a,
                    vec![root_a.join("source.vb")], 1000, &CancellationToken::new(), |_, _| {}).await?;
                ensure!(prepared.started.load(Ordering::SeqCst) && a.count_docs("budget_retry")? > 0
                    && retried.vb_parser_provenance.values().all(|provenance| provenance.is_verified())
                    && budget.breakdown().parse_buffer == 0 && budget.breakdown().misc > 0,
                    "Releasing actual pressure must allow the same VB source to complete without leaked preparation capacity");
                a.parse_preparation_pause = None;
                let b = engine(&owned.path().join("store_b"), budget).await
                    .with_vb_source_inventory(Arc::new(vec![root_b.join("source.vb")]));
                let pause = Arc::new(ParsePause::default());
                let _resume = ResumeOnDrop(pause.clone());
                a.parse_pause = Some(pause.clone());
                let a_engine = a.clone();
                let a_root = root_a.clone();
                let mut request_a = tokio::spawn(async move {
                    a_engine.index_files("project_a", "memory", 1, &a_root,
                        vec![a_root.join("source.vb")], 1000, &CancellationToken::new(), |_, _| {}).await
                });
                ensure!(tokio::time::timeout(Duration::from_secs(20), pause.entered.notified()).await.is_ok(),
                    "Project A must pause inside its actual blocking parse owner");
                request_a.abort();
                ensure!((&mut request_a).await.is_err());
                ensure!(crate::vb_extractor::shutdown_idle_parser().is_err(),
                    "A cancelled waiter must not make its worker-owned compilation look idle");
                let context_free = crate::vb_extractor::extract_vb_with_provenance(&root_b.join("source.vb"), source_b);
                ensure!(!context_free.2.is_verified(), "Context-free extraction must refuse the active batch instead of reusing its root");
                let b_engine = b.clone();
                let b_root = root_b.clone();
                let mut request_b = tokio::spawn(async move {
                    b_engine.index_files("project_b", "memory", 1, &b_root,
                        vec![b_root.join("source.vb")], 1000, &CancellationToken::new(), |_, _| {}).await
                });
                let early = tokio::time::timeout(Duration::from_millis(150), &mut request_b).await;
                let stayed_pending = early.is_err();
                pause.release();
                ensure!(tokio::time::timeout(Duration::from_secs(20), pause.disposed.notified()).await.is_ok());
                let b_stats = match early {
                    Ok(result) => result??,
                    Err(_) => tokio::time::timeout(Duration::from_secs(20), request_b).await??? ,
                };
                ensure!(stayed_pending, "Project B must wait for cancelled A's actual worker disposal");
                ensure!(a.count_docs("project_a")? == 0, "Abandoned A output must not enter source storage");
                ensure!(b_stats.symbols.iter().any(|(_, s)| s.name == "OnlyProjectB")
                    && !b_stats.symbols.iter().any(|(_, s)| s.name == "OnlyProjectA")
                    && b_stats.vb_parser_provenance.values().all(|p| matches!(p, crate::vb_extractor::VbParserProvenance::Roslyn { .. })),
                    "The next real project must parse under its own verified compilation");
                ensure!(b.count_docs("project_b")? > 0);
                drop(a);
                drop(b);

                // Cancellation before ParseWork exists must retain the same
                // global parse capacity in the real preparation worker. A
                // Rust-only peer does not need the VB compilation semaphore,
                // so only retained parse admission can keep it queued.
                let root_c = owned.path().join("project C warmup");
                let root_d = owned.path().join("project D rust");
                std::fs::create_dir(&root_c)?;
                std::fs::create_dir(&root_d)?;
                std::fs::write(root_c.join("source.vb"), "Public Class PreparingC\nEnd Class\n")?;
                std::fs::write(root_d.join("source.rs"), "pub fn admitted_after_warmup() {}\n")?;
                let budget = retained_budget.clone();
                let limiter = Arc::new(tokio::sync::Semaphore::new(1));
                let preparation = Arc::new(ParsePause::default());
                let _resume_preparation = ResumeOnDrop(preparation.clone());
                let mut c = engine(&owned.path().join("store_c"), budget.clone()).await
                    .with_parse_limiter(limiter.clone())?
                    .with_vb_source_inventory(Arc::new(vec![root_c.join("source.vb")]));
                c.parse_preparation_pause = Some(preparation.clone());
                let mut d = engine(&owned.path().join("store_d"), budget.clone()).await
                    .with_parse_limiter(limiter.clone())?;
                let admission = Arc::new(AdmissionProbe::default());
                d.parse_admission_probe = Some(admission.clone());
                let c_engine = c.clone();
                let mut request_c = tokio::spawn(async move {
                    c_engine.index_files("project_c", "memory", 1, &root_c,
                        vec![root_c.join("source.vb")], 1000, &CancellationToken::new(), |_, _| {}).await
                });
                ensure!(tokio::time::timeout(Duration::from_secs(20), preparation.entered.notified()).await.is_ok(),
                    "Cancellation must land in the actual blocking preparation owner");
                request_c.abort();
                ensure!((&mut request_c).await.is_err());
                ensure!(limiter.available_permits() == 0 && budget.breakdown().parse_buffer > 0
                    && crate::vb_extractor::shutdown_idle_parser().is_err(),
                    "Parse admission, inventory reservation and compilation must follow the cancelled preparation worker");
                let d_engine = d.clone();
                let mut request_d = tokio::spawn(index_one(d_engine, root_d));
                ensure!(tokio::time::timeout(Duration::from_secs(20), admission.changed.notified()).await.is_ok()
                    && admission.state.load(Ordering::SeqCst) == 1,
                    "A real Rust-only peer must be queued by retained warmup capacity");
                preparation.release();
                let d_stats = tokio::time::timeout(Duration::from_secs(20), &mut request_d).await???;
                ensure!(d_stats.chunks > 0 && d.count_docs("project")? > 0 && c.count_docs("project_c")? == 0,
                    "Warmup disposal must release capacity and never publish abandoned C source");
                ensure!(budget.breakdown().parse_buffer == 0, "Actual preparation disposal must return its inventory reservation");
                ensure!(budget.breakdown().misc > 0 && first_cache_budget.breakdown().misc == 0,
                    "The cached child must retain its charge under the current budget after request disposal, without retaining the previous budget's cache");
                drop(c);
                drop(d);
                Ok::<(), anyhow::Error>(())
            });
            // Runtime disposal drains any native/blocking owners on early
            // errors before parser shutdown and owned-directory removal.
            drop(runtime);
            tested
        })();
        let stopped = crate::vb_extractor::shutdown_idle_parser();
        drop(owner);
        let cleanup = owned.close();
        result?;
        stopped?;
        ensure!(retained_budget.breakdown().misc == 0 && retained_budget.breakdown().parse_buffer == 0,
            "Joining the actual cached child must release its retained and transient reservations");
        cleanup?;
        Ok(())
    }

    #[tokio::test]
    async fn extractor_panic_preserves_source_and_allows_retry_without_restarting_the_engine() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("source");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("source.rs"),
            "pub fn retry_after_parser_failure() -> usize { 3 }\n",
        )
        .unwrap();
        let budget = Arc::new(MemoryBudget::new(16 * 1024 * 1024));
        let mut index = engine(temporary.path(), budget.clone())
            .await
            .with_parse_limiter(Arc::new(tokio::sync::Semaphore::new(1)))
            .unwrap();
        let failure = Arc::new(ParsePause::default());
        failure.release();
        failure.panic_once.store(true, Ordering::SeqCst);
        index.parse_pause = Some(failure);
        let failed = index_one(index.clone(), root.clone()).await;
        let extraction_failed = failed
            .as_ref()
            .is_err_and(|error| format!("{error:#}").contains("Source extraction panicked"));
        let source_preserved = index.count_docs("project").unwrap_or(usize::MAX) == 0;
        let recovery_available = index.snapshots.mutations.try_recovery().is_ok();
        let retry = index_one(index.clone(), root).await;
        let retry_complete = retry.as_ref().is_ok_and(|stats| stats.chunks > 0)
            && index.count_docs("project").unwrap_or(0) > 0;
        drop(failed);
        drop(retry);
        let reservation_released = budget.breakdown().parse_buffer == 0;
        drop(index);
        temporary.close().unwrap();
        assert!(
            extraction_failed && source_preserved,
            "A parser panic must refuse the update without writing its output"
        );
        assert!(
            recovery_available && retry_complete,
            "An extraction failure must not permanently poison native storage coordination"
        );
        assert!(
            reservation_released,
            "A parser panic must not leak transient capacity"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_parse_keeps_global_capacity_and_root_recovery_blocked_until_actual_disposal()
    {
        let temporary = tempfile::tempdir().unwrap();
        let root_a = temporary.path().join("a");
        let root_b = temporary.path().join("b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(
            root_a.join("source.rs"),
            "pub fn cancelled_source() -> usize { 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root_b.join("source.rs"),
            "pub fn surviving_source() -> usize { 2 }\n",
        )
        .unwrap();
        let budget = Arc::new(MemoryBudget::new(16 * 1024 * 1024));
        let limiter = Arc::new(tokio::sync::Semaphore::new(1));
        let mut a = engine(&root_a, budget.clone())
            .await
            .with_parse_limiter(limiter.clone())
            .unwrap();
        let mut b = engine(&root_b, budget.clone())
            .await
            .with_parse_limiter(limiter.clone())
            .unwrap();
        let pause = Arc::new(ParsePause::default());
        let _resume = ResumeOnDrop(pause.clone());
        let probe = Arc::new(AdmissionProbe::default());
        let b_lifetime = Arc::new(ParsePause::default());
        b_lifetime.release();
        a.parse_pause = Some(pause.clone());
        b.parse_pause = Some(b_lifetime.clone());
        b.parse_admission_probe = Some(probe.clone());
        let mut request_a = tokio::spawn(index_one(a.clone(), root_a));

        // Observe the real extraction entry, not a parent-future checkpoint.
        let a_entered = tokio::time::timeout(Duration::from_secs(15), pause.entered.notified())
            .await
            .is_ok();
        request_a.abort();
        let a_cancelled = (&mut request_a).await.is_err();
        let reservation_survived = budget.breakdown().parse_buffer > 0;
        let root_blocked = matches!(
            a.snapshots.mutations.try_recovery(),
            Err(CoordinationError::Busy)
        );
        let mut request_b = tokio::spawn(index_one(b.clone(), root_b));
        let b_polled_admission =
            tokio::time::timeout(Duration::from_secs(15), probe.changed.notified())
                .await
                .is_ok();
        let b_was_pending = probe.state.load(Ordering::SeqCst) == 1;

        // Always release/join actual work and close stores before assertions.
        pause.release();
        let actual_disposed =
            tokio::time::timeout(Duration::from_secs(15), pause.disposed.notified())
                .await
                .is_ok();
        let b_result = tokio::time::timeout(Duration::from_secs(15), &mut request_b).await;
        let b_succeeded = match b_result {
            Ok(Ok(Ok(stats))) => stats.chunks > 0,
            Ok(_) => false,
            Err(_) => {
                request_b.abort();
                let _ = request_b.await;
                false
            }
        };
        // A failed observation may have aborted B while its blocking task still
        // ran. Drain worker/derived-engine owners before disposing its fixture.
        // The remaining two references belong to this local and engine b.
        while Arc::strong_count(&b_lifetime) > 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let a_unchanged = a.count_docs("project").unwrap_or(usize::MAX) == 0;
        let b_searchable = b.count_docs("project").unwrap_or(0) > 0;
        let root_released = a.snapshots.mutations.try_recovery().is_ok();
        let reservation_released = budget.breakdown().parse_buffer == 0;
        drop(a);
        drop(b);
        temporary.close().unwrap();
        assert!(
            a_entered && a_cancelled,
            "The original indexing request must be cancelled during actual extraction"
        );
        assert!(
            reservation_survived,
            "A cancelled waiter must not free a running parser's reservation"
        );
        assert!(
            b_polled_admission && b_was_pending,
            "An independent project must remain admission-blocked while cancelled parsing still runs"
        );
        assert!(
            root_blocked,
            "Recovery must not overtake the cancelled parser"
        );
        assert!(
            actual_disposed && b_succeeded && b_searchable,
            "Real disposal must release capacity and let the next project finish"
        );
        assert!(
            a_unchanged,
            "Abandoned parse output must not enter the source index"
        );
        assert!(
            root_released && reservation_released,
            "Admission and transient reservation must not leak after disposal"
        );
    }

    #[tokio::test]
    async fn large_binary_inventory_can_progress_without_retaining_all_input_byte_reservations() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("source");
        std::fs::create_dir_all(&root).unwrap();
        let mut paths = Vec::new();
        let content = vec![0u8; 64 * 1024];
        for number in 0..101 {
            let path = root.join(format!("binary{number:03}.rs"));
            std::fs::write(&path, &content).unwrap();
            paths.push(path);
        }
        // A transient parse-reservation progress check, not a hard RSS bound:
        // native stores/retained structured output require separate accounting.
        let budget = Arc::new(MemoryBudget::new(4 * 1024 * 1024));
        let index = engine(temporary.path(), budget.clone()).await;
        let result = index
            .index_files(
                "project",
                "memory",
                1,
                &root,
                paths,
                1000,
                &CancellationToken::new(),
                |_, _| {},
            )
            .await;
        let complete = result
            .as_ref()
            .is_ok_and(|stats| stats.files == 101 && stats.skipped_files.len() == 101);
        drop(result);
        let released = budget.breakdown().parse_buffer == 0;
        drop(index);
        temporary.close().unwrap();
        assert!(
            complete,
            "Total binary source bytes may exceed the transient reservation budget when each live chunk fits"
        );
        assert!(
            released,
            "Completed parsing must release its transient reservations"
        );
    }
}
