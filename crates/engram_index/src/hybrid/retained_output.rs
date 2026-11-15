//! Admission for retained parser output, separate from transient parsing work.
//! Estimates cover owned strings/containers with4x merge-growth allowance.
//! This is accounting/backpressure, not a bound on parser/native heap or RSS.
use super::{AllocationGuard, IndexDoc, IngestStats, MemoryBudget, Subsystem};
use std::{collections::HashMap, fmt, mem::size_of};

#[derive(Default)]
pub struct RetainedIngestOutput {
    guards: Vec<AllocationGuard>,
    bytes: u64,
}
impl fmt::Debug for RetainedIngestOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetainedIngestOutput")
            .field("reserved_bytes", &self.bytes)
            .finish()
    }
}
impl RetainedIngestOutput {
    fn admit(budget: Option<&MemoryBudget>, size: Size, operation: &str) -> anyhow::Result<Self> {
        let Some(budget) = budget else {
            return Ok(Self::default());
        };
        let bytes = size
            .0
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(256))
            .ok_or_else(|| anyhow::anyhow!("Retained source output estimate overflow"))?;
        let guard = AllocationGuard::try_new(budget, bytes, Subsystem::ParseBuffer, operation)?;
        Ok(Self {
            guards: vec![guard],
            bytes,
        })
    }
    pub(super) fn append(&mut self, mut other: Self) {
        // Successful reservations already belong to the same operation's
        // shared budget, so this total cannot exceed its u64 tracked counter.
        self.bytes += other.bytes;
        self.guards.append(&mut other.guards);
    }
}

/// Field order disposes all documents before releasing their reservations.
#[derive(Default)]
pub(super) struct DocumentBatch {
    pub docs: Vec<IndexDoc>,
    admission: RetainedIngestOutput,
}
impl DocumentBatch {
    pub fn admit(docs: Vec<IndexDoc>, budget: Option<&MemoryBudget>) -> anyhow::Result<Self> {
        if docs.is_empty() {
            return Ok(Self::default());
        }
        if budget.is_none() {
            return Ok(Self {
                docs,
                admission: RetainedIngestOutput::default(),
            });
        }
        let mut size = Size::default();
        size.vector(&docs)?;
        for doc in &docs {
            size.add(doc.path.as_str().len())?;
            for value in [
                &doc.language,
                &doc.content,
                &doc.namespace,
                &doc.doc_id,
                &doc.content_hash,
            ] {
                size.string(value)?;
            }
            if let Some(author) = &doc.author {
                size.string(author)?;
            }
        }
        let admission =
            RetainedIngestOutput::admit(budget, size, "retained source document batch")?;
        Ok(Self { docs, admission })
    }
    pub fn append(&mut self, mut other: Self) {
        // Admission precedes destination Vec growth. Its allowance covers the
        // merge's temporary source/destination container overlap.
        self.admission.append(other.admission);
        self.docs.append(&mut other.docs);
    }
    pub fn clear(&mut self) {
        // Empty Vec capacity still owns memory. Dispose it before releasing
        // the document charge, rather than retaining unaccounted headers.
        self.docs = Vec::new();
        self.admission = RetainedIngestOutput::default();
    }
}

#[derive(Default)]
struct Size(u64);
impl Size {
    fn add(&mut self, bytes: usize) -> anyhow::Result<()> {
        self.0 = self
            .0
            .checked_add(u64::try_from(bytes)?)
            .ok_or_else(|| anyhow::anyhow!("Retained source output estimate overflow"))?;
        Ok(())
    }
    fn string(&mut self, value: &String) -> anyhow::Result<()> {
        self.add(value.capacity())
    }
    fn vector<T>(&mut self, values: &Vec<T>) -> anyhow::Result<()> {
        self.add(
            values
                .capacity()
                .checked_mul(size_of::<T>())
                .ok_or_else(|| anyhow::anyhow!("Retained vector estimate overflow"))?,
        )
    }
    fn map<K, V>(&mut self, values: &HashMap<K, V>) -> anyhow::Result<()> {
        self.add(
            values
                .capacity()
                .checked_mul(size_of::<(K, V)>())
                .and_then(|bytes| bytes.checked_mul(2))
                .ok_or_else(|| anyhow::anyhow!("Retained map estimate overflow"))?,
        )
    }
    fn metadata(&mut self, values: &Option<HashMap<String, String>>) -> anyhow::Result<()> {
        if let Some(values) = values {
            self.map(values)?;
            for (key, value) in values {
                self.string(key)?;
                self.string(value)?;
            }
        }
        Ok(())
    }
}

pub(super) fn admit_stats(
    stats: &IngestStats,
    budget: Option<&MemoryBudget>,
) -> anyhow::Result<RetainedIngestOutput> {
    if budget.is_none() {
        return Ok(RetainedIngestOutput::default());
    }
    let mut size = Size::default();
    size.vector(&stats.all_files)?;
    for path in &stats.all_files {
        size.add(path.as_str().len())?;
    }
    size.vector(&stats.fingerprints)?;
    for value in &stats.fingerprints {
        size.string(&value.rel_path)?;
        size.string(&value.file_hash)?;
    }
    size.vector(&stats.symbols)?;
    for (path, value) in &stats.symbols {
        size.add(path.as_str().len())?;
        size.string(&value.name)?;
        size.string(&value.kind)?;
        size.metadata(&value.metadata)?;
    }
    size.vector(&stats.edges)?;
    for (path, value) in &stats.edges {
        size.add(path.as_str().len())?;
        for text in [
            &value.source_name,
            &value.source_kind,
            &value.source_language,
            &value.target_name,
            &value.kind,
        ] {
            size.string(text)?;
        }
        if let Some(kind) = &value.target_kind {
            size.string(kind)?;
        }
        size.metadata(&value.metadata)?;
    }
    size.vector(&stats.skipped_files)?;
    for (path, reason) in &stats.skipped_files {
        size.add(path.as_str().len())?;
        size.string(reason)?;
    }
    size.vector(&stats.warnings)?;
    for value in &stats.warnings {
        size.string(value)?;
    }
    size.map(&stats.languages)?;
    for key in stats.languages.keys() {
        size.string(key)?;
    }
    size.map(&stats.webforms_position_contracts)?;
    for key in stats.webforms_position_contracts.keys() {
        size.string(key)?;
    }
    size.map(&stats.script_call_contracts)?;
    for key in stats.script_call_contracts.keys() {
        size.string(key)?;
    }
    size.map(&stats.vb_parser_provenance)?;
    for (path, provenance) in &stats.vb_parser_provenance {
        size.string(path)?;
        if let Some(context) = provenance.context() {
            if let Some(identity) = &context.roslyn {
                for text in [
                    &identity.contract,
                    &identity.entry_mvid,
                    &identity.roslyn_core_mvid,
                    &identity.roslyn_vb_mvid,
                ] {
                    size.string(text)?;
                }
            }
            if let Some(compiler) = &context.compiler_context {
                for text in [&compiler.root, &compiler.fingerprint, &compiler.mode] {
                    size.string(text)?;
                }
                if let Some(reason) = &compiler.reason {
                    size.string(reason)?;
                }
            }
        }
    }
    RetainedIngestOutput::admit(budget, size, "retained source statistics")
}
