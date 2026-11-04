//! Exclude recovery from complete index mutations, including publication.
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, thiserror::Error)]
pub enum CoordinationError {
    #[error("vector recovery coordination unavailable: {0}")]
    Unavailable(&'static str),
    #[error("vector recovery or mutation is active; retry the operation")]
    Busy,
    #[error("too many concurrent index mutations")]
    MutationCountOverflow,
    #[error("vector recovery reservation counter exhausted")]
    ReservationExhausted,
}

type Result<T> = std::result::Result<T, CoordinationError>;

#[derive(Clone, Copy)]
enum Phase {
    Ready,
    #[cfg(any(feature = "vector", test))]
    Exclusive(u64),
    #[cfg(any(feature = "vector", test))]
    Uncertain,
}

struct State {
    phase: Phase,
    mutations: usize,
    pending_operations: usize,
    #[cfg(any(feature = "vector", test))]
    next_reservation: u64,
    faulted: bool,
}

pub(super) struct Coordinator {
    state: Mutex<State>,
    pub(super) commit_lane: Arc<tokio::sync::Mutex<()>>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            commit_lane: Arc::new(tokio::sync::Mutex::new(())),
            state: Mutex::new(State {
                phase: Phase::Ready,
                mutations: 0,
                pending_operations: 0,
                #[cfg(any(feature = "vector", test))]
                next_reservation: 0,
                faulted: false,
            }),
        }
    }
}

impl Coordinator {
    fn checked_state(&self) -> Result<MutexGuard<'_, State>> {
        let state = self
            .state
            .lock()
            .map_err(|_| CoordinationError::Unavailable("coordination lock poisoned"))?;
        if state.faulted {
            return Err(CoordinationError::Unavailable(
                "coordination invariant violated",
            ));
        }
        Ok(state)
    }

    pub(super) fn try_mutation(self: &Arc<Self>) -> Result<MutationPermit> {
        let mut state = self.checked_state()?;
        if !matches!(state.phase, Phase::Ready) || state.pending_operations != 0 {
            return Err(CoordinationError::Busy);
        }
        state.mutations = state
            .mutations
            .checked_add(1)
            .ok_or(CoordinationError::MutationCountOverflow)?;
        Ok(MutationPermit {
            coordinator: self.clone(),
        })
    }

    /// Block newly admitted retries while an already-admitted native write or
    /// publication finishes. Its server serialization lock may have been lost
    /// with the requesting future, but its storage work must not be overtaken.
    pub(super) fn try_operation(self: &Arc<Self>) -> Result<OperationPermit> {
        let mut state = self.checked_state()?;
        if !matches!(state.phase, Phase::Ready) || state.pending_operations != 0 {
            return Err(CoordinationError::Busy);
        }
        self.reserve_operation(&mut state)
    }

    pub(super) fn validate_mutation(self: &Arc<Self>, permit: &MutationPermit) -> Result<()> {
        if !Arc::ptr_eq(self, &permit.coordinator) {
            return Err(CoordinationError::Unavailable(
                "foreign mutation capability",
            ));
        }
        let state = self.checked_state()?;
        if !matches!(state.phase, Phase::Ready) || state.mutations == 0 {
            return Err(CoordinationError::Busy);
        }
        Ok(())
    }

    /// Existing owners may finish while another admitted worker is pending.
    /// An unrelated caller cannot use this path without the live capability.
    pub(super) fn operation_within(
        self: &Arc<Self>,
        permit: &MutationPermit,
    ) -> Result<OperationPermit> {
        if !Arc::ptr_eq(self, &permit.coordinator) {
            return Err(CoordinationError::Unavailable(
                "foreign mutation capability",
            ));
        }
        let mut state = self.checked_state()?;
        if !matches!(state.phase, Phase::Ready) || state.mutations == 0 {
            return Err(CoordinationError::Busy);
        }
        self.reserve_operation(&mut state)
    }

    fn reserve_operation(self: &Arc<Self>, state: &mut State) -> Result<OperationPermit> {
        let pending = state
            .pending_operations
            .checked_add(1)
            .ok_or(CoordinationError::MutationCountOverflow)?;
        state.mutations = state
            .mutations
            .checked_add(1)
            .ok_or(CoordinationError::MutationCountOverflow)?;
        state.pending_operations = pending;
        Ok(OperationPermit {
            coordinator: self.clone(),
        })
    }

    pub(super) fn mark_faulted(&self) {
        self.state_for_drop().faulted = true;
    }

    #[cfg(any(feature = "vector", test))]
    pub(super) fn try_recovery(self: &Arc<Self>) -> Result<RecoveryPermit> {
        self.reserve_recovery(false)
    }

    /// Only authoritative route validation may reopen uncertain publication.
    #[cfg(feature = "vector")]
    pub(super) fn try_resume_uncertain(self: &Arc<Self>) -> Result<RecoveryPermit> {
        self.reserve_recovery(true)
    }

    #[cfg(any(feature = "vector", test))]
    fn reserve_recovery(self: &Arc<Self>, resume: bool) -> Result<RecoveryPermit> {
        let mut state = self.checked_state()?;
        let allowed = matches!(
            (resume, state.phase),
            (false, Phase::Ready) | (true, Phase::Uncertain)
        );
        if !allowed || state.mutations != 0 || state.pending_operations != 0 {
            return Err(CoordinationError::Busy);
        }
        let reservation = state
            .next_reservation
            .checked_add(1)
            .ok_or(CoordinationError::ReservationExhausted)?;
        state.next_reservation = reservation;
        state.phase = Phase::Exclusive(reservation);
        Ok(RecoveryPermit {
            coordinator: self.clone(),
            reservation,
            publication_possible: resume,
            live: true,
        })
    }

    // Release resources during unwinding without resetting a poisoned lock.
    fn state_for_drop(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.faulted = true;
                state
            }
        }
    }
}

#[must_use]
pub(super) struct MutationPermit {
    coordinator: Arc<Coordinator>,
}

impl Drop for MutationPermit {
    fn drop(&mut self) {
        let mut state = self.coordinator.state_for_drop();
        if !matches!(state.phase, Phase::Ready) {
            state.faulted = true;
        }
        if state.mutations == 0 {
            state.faulted = true;
        } else {
            state.mutations -= 1;
        }
    }
}

#[must_use]
pub(super) struct OperationPermit {
    coordinator: Arc<Coordinator>,
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        let mut state = self.coordinator.state_for_drop();
        if !matches!(state.phase, Phase::Ready)
            || state.pending_operations == 0
            || state.mutations == 0
            || std::thread::panicking()
        {
            state.faulted = true;
        }
        if state.pending_operations > 0 {
            state.pending_operations -= 1;
        }
        if state.mutations > 0 {
            state.mutations -= 1;
        }
    }
}

#[must_use]
#[cfg(any(feature = "vector", test))]
pub(super) struct RecoveryPermit {
    coordinator: Arc<Coordinator>,
    reservation: u64,
    publication_possible: bool,
    live: bool,
}

#[cfg(feature = "vector")]
impl RecoveryPermit {
    /// Call before an atomic routing/table publication might take effect.
    pub(super) fn mark_publication_possible(&mut self) -> Result<()> {
        let mut state = self.coordinator.checked_state()?;
        if !matches!(state.phase, Phase::Exclusive(id) if id == self.reservation)
            || state.mutations != 0
        {
            state.faulted = true;
            return Err(CoordinationError::Unavailable(
                "exclusive recovery ownership lost",
            ));
        }
        self.publication_possible = true;
        Ok(())
    }

    /// Durable publication and in-memory activation must both be complete.
    pub(super) fn finish_activation(mut self) -> Result<()> {
        {
            let mut state = self.coordinator.checked_state()?;
            if !matches!(state.phase, Phase::Exclusive(id) if id == self.reservation)
                || state.mutations != 0
            {
                state.faulted = true;
                return Err(CoordinationError::Unavailable(
                    "exclusive recovery ownership lost",
                ));
            }
            state.phase = Phase::Ready;
            self.live = false;
        }
        Ok(())
    }
}

#[cfg(any(feature = "vector", test))]
impl Drop for RecoveryPermit {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut state = self.coordinator.state_for_drop();
        if !matches!(state.phase, Phase::Exclusive(id) if id == self.reservation)
            || state.mutations != 0
        {
            state.faulted = true;
            return;
        }
        state.phase = if self.publication_possible {
            Phase::Uncertain
        } else {
            Phase::Ready
        };
    }
}
