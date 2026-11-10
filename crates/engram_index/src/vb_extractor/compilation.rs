//! Bounded compilation input and estimated retained tree accounting. The
//! multiplier is admission policy, not a hard Roslyn process/RSS ceiling.
use engram_core::memory::{AllocationGuard, MemoryBudget, Subsystem};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, Mutex};

pub(super) static DEFAULT_BUDGET: LazyLock<Arc<MemoryBudget>> =
    LazyLock::new(|| Arc::new(MemoryBudget::new(0)));

pub(super) const MAX_REFERENCE_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn reference_charge(bytes: u64) -> anyhow::Result<u64> {
    anyhow::ensure!(bytes <= MAX_REFERENCE_BYTES, "VB framework reference limit exceeded");
    Ok(bytes * 4)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilationPolicy {
    pub max_total_source_bytes: u64,
    pub max_file_source_bytes: u64,
    pub max_trees: u64,
    pub allocation_multiplier: u64,
    pub tree_overhead_bytes: u64,
}

impl Default for CompilationPolicy {
    fn default() -> Self {
        Self { max_total_source_bytes: 32 * 1024 * 1024, max_file_source_bytes: crate::ingest::MAX_FILE_SIZE,
            max_trees: 250_000, allocation_multiplier: 32, tree_overhead_bytes: 8192 }
    }
}

impl CompilationPolicy {
    pub(super) fn supported(&self) -> bool {
        let maximum = Self::default();
        self.max_total_source_bytes > 0 && self.max_total_source_bytes <= maximum.max_total_source_bytes
            && self.max_file_source_bytes > 0 && self.max_file_source_bytes <= maximum.max_file_source_bytes
            && self.max_trees > 0 && self.max_trees <= maximum.max_trees
            && self.allocation_multiplier == maximum.allocation_multiplier
            && self.tree_overhead_bytes == maximum.tree_overhead_bytes
    }

    pub(super) fn matches_metadata(&self, value: &serde_json::Value) -> bool {
        value.as_object().is_some_and(|object| object.len() == 5
            && object.get("max_total_source_bytes").and_then(|v| v.as_u64()) == Some(self.max_total_source_bytes)
            && object.get("max_file_source_bytes").and_then(|v| v.as_u64()) == Some(self.max_file_source_bytes)
            && object.get("max_trees").and_then(|v| v.as_u64()) == Some(self.max_trees)
            && object.get("allocation_multiplier").and_then(|v| v.as_u64()) == Some(self.allocation_multiplier)
            && object.get("tree_overhead_bytes").and_then(|v| v.as_u64()) == Some(self.tree_overhead_bytes))
    }

    pub(super) fn charge(&self, bytes: u64, trees: u64) -> anyhow::Result<u64> {
        anyhow::ensure!(self.supported() && bytes <= self.max_total_source_bytes && trees <= self.max_trees,
            "VB compilation source/count limit exceeded; narrow the admitted project (request source limits are separate)");
        bytes.checked_mul(self.allocation_multiplier).and_then(|bytes|
            trees.checked_mul(self.tree_overhead_bytes).and_then(|nodes| bytes.checked_add(nodes)))
            .ok_or_else(|| anyhow::anyhow!("VB compilation allocation estimate overflow"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub id: u64,
    pub total_source_bytes: u64,
    pub tree_count: u64,
    pub changed_source_bytes: u64,
    pub changed_tree_count: u64,
    pub max_changed_file_bytes: u64,
    pub full: bool,
    pub policy: CompilationPolicy,
    #[serde(default)]
    pub reference_bytes: u64,
    #[serde(default)]
    pub compiler_context: Option<super::VbCompilerContext>,
}

impl Plan {
    pub(super) fn charges(&self, policy: &CompilationPolicy) -> anyhow::Result<(u64, u64)> {
        anyhow::ensure!(self.id != 0 && &self.policy == policy && self.changed_tree_count <= self.tree_count
            && self.changed_source_bytes <= self.total_source_bytes
            && self.max_changed_file_bytes <= policy.max_file_source_bytes
            && self.max_changed_file_bytes <= self.changed_source_bytes
            && (self.tree_count != 0 || self.total_source_bytes == 0)
            && (self.changed_tree_count != 0 || self.changed_source_bytes == 0)
            && (!self.full || (self.changed_tree_count == self.tree_count && self.changed_source_bytes == self.total_source_bytes)),
            "VB compilation plan does not match its admitted policy");
        anyhow::ensure!(self.compiler_context.as_ref().map_or(0, |c| c.reference_bytes) == self.reference_bytes,
            "VB compilation reference charge does not match compiler context");
        let references = reference_charge(self.reference_bytes)?;
        let retained = policy.charge(self.total_source_bytes, self.tree_count)?.checked_add(references)
            .ok_or_else(|| anyhow::anyhow!("VB compilation metadata estimate overflow"))?;
        let changed = if self.full { 0 } else { policy.charge(self.changed_source_bytes, self.changed_tree_count)? };
        let scratch = self.max_changed_file_bytes.checked_mul(4).and_then(|input| input.checked_add(changed))
            .and_then(|input| input.checked_add(self.reference_bytes * 2))
            .ok_or_else(|| anyhow::anyhow!("VB compilation scratch estimate overflow"))?;
        Ok((retained, scratch))
    }
}

pub(super) struct CachedCharge {
    budget: Arc<MemoryBudget>,
    state: Mutex<(u64, Vec<AllocationGuard>)>,
}

impl CachedCharge {
    pub(super) fn new(budget: Arc<MemoryBudget>, bytes: u64) -> anyhow::Result<Arc<Self>> {
        Self::reserve(budget, bytes, Subsystem::Misc)
    }

    pub(super) fn scratch(budget: Arc<MemoryBudget>, bytes: u64) -> anyhow::Result<Arc<Self>> {
        Self::reserve(budget, bytes, Subsystem::ParseBuffer)
    }

    fn reserve(budget: Arc<MemoryBudget>, bytes: u64, subsystem: Subsystem) -> anyhow::Result<Arc<Self>> {
        let guard = AllocationGuard::try_new(&budget, bytes, subsystem, "VB compiled cache/scratch")?;
        Ok(Arc::new(Self { budget, state: Mutex::new((bytes, vec![guard])) }))
    }

    pub(super) fn same_budget(&self, budget: &Arc<MemoryBudget>) -> bool { Arc::ptr_eq(&self.budget, budget) }

    /// Bounded high-water chunks avoid one retained guard per byte-sized edit
    /// without doubling a large cache's charge for a tiny source change.
    pub(super) fn ensure(&self, required: u64, maximum: u64) -> anyhow::Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("VB cache accounting unavailable"))?;
        if required > state.0 {
            anyhow::ensure!(state.1.len() < 4096, "VB cache accounting needs an idle cache restart");
            let spare = (required / 16).clamp(4096, 1024 * 1024);
            let target = required.max(state.0.checked_add(spare).unwrap_or(maximum).min(maximum));
            let guard = AllocationGuard::try_new(&self.budget, target - state.0, Subsystem::Misc,
                "VB compiled cache growth")?;
            state.1.push(guard);
            state.0 = target;
        }
        Ok(())
    }
}
