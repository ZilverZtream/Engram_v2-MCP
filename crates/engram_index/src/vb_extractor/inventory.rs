//! Admission for derived compilation metadata, before cloning an admitted hint.
use engram_core::memory::{AllocationGuard, MemoryBudget, Subsystem};
use std::path::{Component, Path, PathBuf};

const MAX_ENTRIES: usize = 250_000;
const MAX_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_WIRE_BYTES: usize = 32 * 1024 * 1024;

pub(super) struct PreparationMemory {
    // Both reservations follow the blocking owner and its returned lease.
    _metadata: Option<AllocationGuard>,
    prepared: Option<AllocationGuard>,
    raw_path_bytes: usize,
}

fn add(left: usize, right: usize) -> anyhow::Result<usize> {
    left.checked_add(right).ok_or_else(|| anyhow::anyhow!("VB inventory allocation size overflow"))
}

fn mul(left: usize, right: usize) -> anyhow::Result<usize> {
    left.checked_mul(right).ok_or_else(|| anyhow::anyhow!("VB inventory allocation size overflow"))
}

fn text(path: &Path) -> anyhow::Result<&str> {
    path.to_str().ok_or_else(|| anyhow::anyhow!("VB compilation inventory requires UTF-8 paths"))
}

fn relative<'a>(root: &Path, path: &'a Path) -> anyhow::Result<&'a str> {
    let relative = path.strip_prefix(root).map_err(|_| anyhow::anyhow!("VB inventory source outside admitted root"))?;
    anyhow::ensure!(!relative.components().any(|part| matches!(part, Component::ParentDir)),
        "VB inventory source escapes its admitted root");
    text(relative)
}

fn escaped_bytes(text: &str) -> anyhow::Result<usize> {
    text.bytes().try_fold(0, |bytes, byte| add(bytes, match byte {
        b'"' | b'\\' => 2,
        0..=31 => 6,
        _ => 1,
    }))
}

fn reserve(budget: Option<&MemoryBudget>, bytes: usize) -> anyhow::Result<Option<AllocationGuard>> {
    budget.map(|budget| AllocationGuard::try_new(budget, bytes as u64, Subsystem::ParseBuffer,
        "VB compilation inventory/preparation")).transpose()
}

impl PreparationMemory {
    /// Borrowed-only validation precedes relative strings, changed-path clones,
    /// hash buckets, sorted pointers, or a parser probe. No filesystem calls.
    pub(super) fn admit(root: &Path, files: &[PathBuf], inventory: &[PathBuf],
        budget: Option<&MemoryBudget>) -> anyhow::Result<Self> {
        anyhow::ensure!(files.len() <= MAX_ENTRIES && inventory.len() <= MAX_ENTRIES,
            "VB compilation inventory exceeds its entry limit");
        let root_bytes = text(root)?.len();
        let mut bytes = root_bytes;
        let changed = files.iter().filter(|path| super::is_vb_path(path)).count();
        for path in inventory.iter().chain(files.iter().filter(|path| super::is_vb_path(path))) {
            relative(root, path)?;
            bytes = add(bytes, text(path)?.len())?;
            anyhow::ensure!(bytes <= MAX_PATH_BYTES, "VB compilation inventory exceeds its path-byte limit");
        }
        // Relative strings/components, changed strings, hash-table slack and
        // sorted references are bounded from the actual admitted input size.
        let metadata = add(add(mul(bytes, 12)?, mul(add(inventory.len(), changed)?, 256)?)?, 4096)?;
        Ok(Self { _metadata: reserve(budget, metadata)?, prepared: None, raw_path_bytes: bytes })
    }

    /// Canonicalize the root once, then reserve its exact possible expansion
    /// and the full escaped wire capacity before derived paths/serialization.
    pub(super) fn reserve_prepared(&mut self, input_root: &Path, canonical_root: &Path,
        inventory: &[PathBuf], changed: &[String], budget: Option<&MemoryBudget>) -> anyhow::Result<()> {
        let canonical = text(canonical_root)?;
        let canonical_escaped = escaped_bytes(canonical)?;
        let mut joined_bytes = canonical.len();
        let mut wire_bytes = add(512, canonical_escaped)?;
        for path in inventory.iter().map(PathBuf::as_path).chain(changed.iter().map(Path::new)) {
            let relative = relative(input_root, path)?;
            joined_bytes = add(joined_bytes, add(add(canonical.len(), 1)?, relative.len())?)?;
            anyhow::ensure!(joined_bytes <= MAX_PATH_BYTES, "VB prepared inventory exceeds its path-byte limit");
            // Windows canonical paths may spell lexical '/' separators as
            // JSON-escaped '\\'; include that possible extra byte too.
            let relative_escaped = add(escaped_bytes(relative)?, relative.bytes().filter(|byte| *byte == b'/').count())?;
            wire_bytes = add(wire_bytes, add(add(canonical_escaped, relative_escaped)?, 8)?)?;
            anyhow::ensure!(wire_bytes <= MAX_WIRE_BYTES, "VB compilation inventory exceeds its wire limit");
        }
        let wire_capacity = wire_bytes.checked_next_power_of_two()
            .ok_or_else(|| anyhow::anyhow!("VB inventory wire capacity overflow"))?
            .max(4096).min(MAX_WIRE_BYTES);
        let expansion = joined_bytes.saturating_sub(self.raw_path_bytes);
        self.prepared = reserve(budget, add(mul(expansion, 12)?, wire_capacity)?)?;
        Ok(())
    }
}
