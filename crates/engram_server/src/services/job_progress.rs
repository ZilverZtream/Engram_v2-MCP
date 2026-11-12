//! Bounded, durable phase observations for existing background jobs.
use engram_core::{
    Registry,
    registry::{JobProgress, JobRecord},
};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Reporter {
    registry: Arc<Registry>,
    job_id: String,
    state: Arc<Mutex<Option<(JobProgress, u64)>>>,
}

impl Reporter {
    pub fn new(registry: Arc<Registry>, job_id: &str) -> Self {
        Self {
            registry,
            job_id: job_id.into(),
            state: Arc::new(Mutex::new(None)),
        }
    }

    pub fn phase(&self, phase: &str, total: Option<u64>, unit: Option<&str>) {
        self.observe_at(phase, total.map(|_| 0), total, unit, crate::utils::now_ms());
    }

    pub fn advance(&self, phase: &str, done: usize, total: usize, unit: &str) {
        self.observe_at(
            phase,
            Some(done as u64),
            Some(total as u64),
            Some(unit),
            crate::utils::now_ms(),
        );
    }

    /// Explicit time also permits deterministic checks of clock changes and ETA.
    pub fn observe_at(
        &self,
        phase: &str,
        done: Option<u64>,
        total: Option<u64>,
        unit: Option<&str>,
        now: u64,
    ) {
        let Ok(mut state) = self.state.lock() else {
            tracing::warn!(job_id=%self.job_id, "Job progress lock unavailable");
            return;
        };
        let previous = state.as_ref().map(|(p, _)| p);
        let same = previous.is_some_and(|p| {
            p.phase == phase
                && p.unit.as_deref() == unit
                && p.completed
                    .zip(done)
                    .is_none_or(|(before, after)| after >= before)
                && now >= p.observed_at_ms
        });
        // Persist phase changes, completed phases and at most one intermediate
        // observation per second. Identical observations don't cause writes.
        if same
            && previous.is_some_and(|p| {
                (p.completed == done && p.total == total)
                    || (now.saturating_sub(p.observed_at_ms) < 1000 && done != total)
            })
        {
            return;
        }
        let started = if same {
            previous.map_or(now, |p| p.started_at_ms)
        } else {
            now
        };
        let baseline = if same {
            state.as_ref().map_or(0, |(_, base)| *base)
        } else {
            done.unwrap_or(0)
        };
        let valid = done.zip(total).filter(|(d, t)| d <= t && *t > 0);
        let percent = valid.map(|(d, t)| ((u128::from(d) * 100) / u128::from(t)) as u8);
        let eta = valid.and_then(|(d, t)| {
            let elapsed = now.checked_sub(started)?;
            let processed = d.checked_sub(baseline)?;
            if processed == 0 || elapsed < 1000 {
                return None;
            }
            u64::try_from(u128::from(elapsed) * u128::from(t - d) / u128::from(processed)).ok()
        });
        let observation = JobProgress {
            phase: phase.into(),
            completed: done,
            total,
            unit: unit.map(str::to_owned),
            started_at_ms: started,
            observed_at_ms: now,
            percent,
            estimated_time_remaining_ms: eta,
        };
        match self.registry.put_job_progress(&self.job_id, &observation) {
            Ok(true) => *state = Some((observation, baseline)),
            Ok(false) => {} // Job ended or was removed; never recreate it.
            Err(error) => {
                tracing::warn!(job_id=%self.job_id, %error, "Job progress could not be persisted")
            }
        }
    }
}

pub fn describe(job: &JobRecord, progress: Option<&JobProgress>) -> serde_json::Value {
    let running = job.status == "running";
    let complete = matches!(job.status.as_str(), "done" | "degraded");
    serde_json::json!({
        "job_complete": complete,
        "phase": progress.map(|p| p.phase.as_str()),
        "phase_state": if running { "current_observation" } else { "last_observation" },
        "completed": progress.and_then(|p| p.completed),
        "total": progress.and_then(|p| p.total),
        "unit": progress.and_then(|p| p.unit.as_deref()),
        "phase_progress_pct": progress.and_then(|p| p.percent),
        "phase_started_at_ms": progress.map(|p| p.started_at_ms),
        "observed_at_ms": progress.map(|p| p.observed_at_ms),
        "job_updated_at_ms": job.updated_at_ms,
        "estimated_time_remaining_ms": if running { progress.and_then(|p| p.estimated_time_remaining_ms) } else { None },
        "estimate_scope": "Current phase at observation time only; not whole-job ETA. Later flush, graph, publication and enrichment work may have unknown duration. A phase at100% does not mean the job is complete.",
        "coverage": if progress.is_some() { "persisted_phase_observation" } else { "phase_unavailable_for_this_job" },
    })
}
