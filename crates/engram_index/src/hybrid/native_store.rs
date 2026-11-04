//! One replaceable connection/Session-wrapper owner for engine clones.
//! Cache rotation changes neither source heads nor physical table identities.
use super::*;
use std::sync::{Mutex, Weak};

pub(super) struct Generation {
    pub connection: lancedb::Connection,
    pub session: Arc<lancedb::Session>,
    pub number: u64,
}

struct Outstanding {
    session: Weak<lancedb::Session>,
    candidate: bool,
}

struct State {
    current: Arc<Generation>,
    // Track at most one prior/candidate Session wrapper. This is NOT a bound
    // on independent Session::clone or dataset/index cache-handle ownership.
    outstanding: Option<Outstanding>,
}

pub(super) struct Store {
    state: Mutex<State>,
    #[cfg(test)]
    pub fail_candidate: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub post_open:
        Mutex<Option<Box<dyn FnOnce(Arc<lancedb::Session>) -> anyhow::Result<()> + Send>>>,
}

impl Store {
    pub fn new(connection: lancedb::Connection, session: Arc<lancedb::Session>) -> Self {
        Self {
            state: Mutex::new(State {
                current: Arc::new(Generation {
                    connection,
                    session,
                    number: 0,
                }),
                outstanding: None,
            }),
            #[cfg(test)]
            fail_candidate: Default::default(),
            #[cfg(test)]
            post_open: Default::default(),
        }
    }

    /// Call only inside an admitted native worker or a live native Epoch /
    /// existing mutation/recovery owner. Do not capture before waiting for it.
    pub fn capture(&self) -> anyhow::Result<Arc<Generation>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Native cache owner unavailable"))?
            .current
            .clone())
    }

    pub fn observation(&self) -> serde_json::Value {
        let Ok(state) = self.state.try_lock() else {
            return serde_json::json!({"status":"busy_or_unavailable"});
        };
        serde_json::json!({"status":"compiled",
            "approximate_session_items":state.current.session.approx_num_items(),
            "session_generation":state.current.number,
            "retired_session_wrapper_owners":state.outstanding.as_ref().map_or(0, |old| usize::from(!old.candidate && old.session.strong_count() > 0)),
            "candidate_session_wrapper_owners":state.outstanding.as_ref().map_or(0, |old| usize::from(old.candidate && old.session.strong_count() > 0)),
            "index_budget_bytes":64*1024*1024,"metadata_budget_bytes":32*1024*1024,
            "scope":"Current approximate items and observed Session-wrapper strong owners only, not all cache handles, bytes or RSS. Session::clone and dataset subcaches can retain cache handles independently. No maintenance/stat getters run."})
    }

    pub fn ensure_no_failed_candidate(&self) -> anyhow::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Native cache owner unavailable"))?;
        anyhow::ensure!(
            !state
                .outstanding
                .as_ref()
                .is_some_and(|old| old.candidate && old.session.strong_count() > 0),
            "A failed native candidate still has an SDK owner; history/cache maintenance deferred"
        );
        Ok(())
    }

    fn may_rotate(&self) -> anyhow::Result<u64> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Native cache owner unavailable"))?;
        anyhow::ensure!(
            !state
                .outstanding
                .as_ref()
                .is_some_and(|old| old.session.strong_count() > 0),
            "A prior native session or candidate still has an SDK owner; further rotation deferred"
        );
        state
            .current
            .number
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Native session generation exhausted"))
    }

    fn reserve_candidate(
        &self,
        expected: u64,
        session: &Arc<lancedb::Session>,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Native cache owner unavailable"))?;
        anyhow::ensure!(
            state.current.number == expected
                && !state
                    .outstanding
                    .as_ref()
                    .is_some_and(|old| old.session.strong_count() > 0),
            "Another native session or candidate still has an owner; candidate not started"
        );
        // Record before connect/open/validation. On ANY later error, SDK-held
        // candidate owners stay observable and prevent repeated generations.
        state.outstanding = Some(Outstanding {
            session: Arc::downgrade(session),
            candidate: true,
        });
        Ok(())
    }

    fn publish(&self, expected: u64, candidate: Arc<Generation>) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Native cache owner unavailable"))?;
        let reserved = state.outstanding.as_ref().is_some_and(|old| {
            old.candidate && Weak::ptr_eq(&old.session, &Arc::downgrade(&candidate.session))
        });
        anyhow::ensure!(
            state.current.number == expected && reserved,
            "Native cache owner changed during maintenance; candidate not installed"
        );
        // All checks precede this single state transition: candidate becomes
        // current and the same bounded outstanding slot follows the old owner.
        let old = std::mem::replace(&mut state.current, candidate);
        state.outstanding = Some(Outstanding {
            session: Arc::downgrade(&old.session),
            candidate: false,
        });
        drop(old);
        Ok(())
    }
}

impl HybridSearchEngine {
    /// Caller retains the engine-wide writer exclusion and the global native
    /// Epoch admission gate through this entire uncancelled worker.
    pub(super) async fn rotate_native_session_after_prune(
        &self,
        owner: &vector_epochs::OwnedTable,
        latest: u64,
    ) -> NativeSessionRotationOutcome {
        let started = std::time::Instant::now();
        let attempt = async {
            let next = self.native_store.may_rotate()?;
            let session = Arc::new(lancedb::Session::new(
                64 * 1024 * 1024,
                32 * 1024 * 1024,
                Arc::new(lancedb::ObjectStoreRegistry::default()),
            ));
            self.native_store.reserve_candidate(next - 1, &session)?;
            #[cfg(test)]
            if self
                .native_store
                .fail_candidate
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                anyhow::bail!(
                    "Controlled candidate connection failure; current cache owner retained"
                );
            }
            // Same root, provider/table identity and existing weighted budgets.
            // This is a fresh cache generation, not reduced cache capacities.
            let connection = lancedb::connect(self._lance_dir.to_string_lossy().as_ref())
                .session(session.clone())
                .execute()
                .await?;
            let table = connection.open_table(&owner.table).execute().await?;
            #[cfg(test)]
            {
                let hook = self
                    .native_store
                    .post_open
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Candidate test hook unavailable"))?
                    .take();
                if let Some(hook) = hook {
                    hook(session.clone())?;
                }
            }
            self.vector_routes.read_owned(owner)?;
            owner.validate_table(&table).await?;
            anyhow::ensure!(
                table.version().await? == latest,
                "Native version changed before cache rotation; candidate not installed"
            );
            drop(table);
            self.native_store.publish(
                next - 1,
                Arc::new(Generation {
                    connection,
                    session,
                    number: next,
                }),
            )?;
            Ok::<_, anyhow::Error>(next)
        }
        .await;
        match attempt {
            Ok(generation) => NativeSessionRotationOutcome::Rotated {
                generation,
                elapsed_ms: started.elapsed().as_millis(),
            },
            Err(error) => NativeSessionRotationOutcome::Deferred {
                reason: format!("{error:#}"),
                elapsed_ms: started.elapsed().as_millis(),
            },
        }
    }
}

#[cfg(test)]
mod tests;
