//! Bounded physical generation discovery for damaged-head recovery.
//! Callers retain project update serialization through reservation/publication.
use super::*;
use std::time::{Duration, Instant};
use tantivy::{DocSet, TERMINATED};

const DEADLINE: Duration = Duration::from_secs(10);
const MAX_SEGMENTS: usize = 256;
const MAX_DOCUMENTS: u64 = 1_000_000;
#[cfg(feature = "vector")]
const MAX_VECTOR_ROWS: usize = 1_000_000;
const MAX_WORK: u64 = 4_000_000;
pub(super) const MAX_METADATA_ENTRIES: usize = 4096;

pub(super) struct WorkBudget<'a> {
    cancel: &'a CancellationToken,
    started: Instant,
    remaining: u64,
}

impl<'a> WorkBudget<'a> {
    pub(super) fn new(cancel: &'a CancellationToken) -> Self {
        Self { cancel, started: Instant::now(), remaining: MAX_WORK }
    }

    pub(super) fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancel.is_cancelled(), "Source recovery generation observation cancelled");
        anyhow::ensure!(self.started.elapsed() < DEADLINE,
            "Source recovery inspection deadline exceeded (10 seconds); source unchanged");
        Ok(())
    }

    pub(super) fn charge(&mut self, amount: u64) -> anyhow::Result<()> {
        self.check()?;
        self.remaining = self.remaining.checked_sub(amount).ok_or_else(|| anyhow::anyhow!(
            "Source recovery inspection work budget exceeded; source unchanged"))?;
        Ok(())
    }

    fn remaining_time(&self) -> anyhow::Result<Duration> {
        self.check()?;
        Ok(DEADLINE.saturating_sub(self.started.elapsed()))
    }

    #[cfg(test)]
    pub(super) fn with_work_limit(cancel: &'a CancellationToken, remaining: u64) -> Self {
        Self { remaining, ..Self::new(cancel) }
    }
}

impl HybridSearchEngine {
    /// Complete physical/catalog floor, including abandoned and leased generations.
    /// This is observation, not allocation: retain the server project update lock
    /// and mutation admission until durable reservation and publication finish.
    pub async fn source_generation_high_water(
        &self, project: &str, cancel: &CancellationToken,
    ) -> anyhow::Result<u64> {
        let mutation = self.begin_project_mutation(project)?;
        let scoped = self.with_mutation(&mutation)?;
        let mut budget = WorkBudget::new(cancel);
        let _commit = tokio::time::timeout(budget.remaining_time()?, scoped.acquire_commit(cancel)).await
            .map_err(|_| anyhow::anyhow!("Source recovery generation observation timed out waiting for storage work"))??;
        scoped.source_high_water_inner(project, &mut budget).await
    }

    pub(super) async fn source_high_water_inner(
        &self, project: &str, budget: &mut WorkBudget<'_>,
    ) -> anyhow::Result<u64> {
        engram_core::security::validate_key_component("project", project).map_err(anyhow::Error::msg)?;
        budget.check()?;
        let mut maximum = self.lexical_source_high_water(project, budget)?;
        maximum = maximum.max(self.vector_source_high_water(project, budget).await?);
        maximum = maximum.max(self.snapshots.recovery_high_water(project, "memory", budget)?);
        budget.check()?;
        Ok(maximum)
    }

    fn lexical_source_high_water(&self, project: &str, budget: &mut WorkBudget<'_>) -> anyhow::Result<u64> {
        anyhow::ensure!(matches!(self.tantivy_index.schema().get_field_entry(self.fields.generation).field_type(),
            tantivy::schema::FieldType::U64(_)), "Source recovery found an unsupported lexical generation schema");
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        anyhow::ensure!(searcher.segment_readers().len() <= MAX_SEGMENTS,
            "Source recovery inspection budget exceeded (256 lexical segments); source unchanged");
        let total_docs: u64 = searcher.segment_readers().iter().map(|segment| u64::from(segment.max_doc())).sum();
        anyhow::ensure!(total_docs <= MAX_DOCUMENTS,
            "Source recovery inspection budget exceeded (1000000 lexical document slots); source unchanged");
        let mut maximum = 0;
        for segment in searcher.segment_readers() {
            budget.check()?;
            // One byte per physical slot, reserved before allocation. No stored
            // source text is decoded: 1=project, 2=live source, 3=generation seen.
            let bytes = u64::from(segment.max_doc());
            let _allocation = self.memory_budget.as_ref().map(|memory| AllocationGuard::try_new(
                memory, bytes, Subsystem::Tantivy, "source recovery generation membership")).transpose()?;
            let mut members = vec![0u8; segment.max_doc() as usize];
            let projects = segment.inverted_index(self.fields.project_id)?;
            if let Some(mut postings) = projects.read_postings(
                &Term::from_field_text(self.fields.project_id, project), IndexRecordOption::Basic)? {
                while postings.doc() != TERMINATED {
                    budget.charge(1)?;
                    let doc = postings.doc();
                    if !segment.is_deleted(doc) { members[doc as usize] = 1; }
                    postings.advance();
                }
            }
            let namespaces = segment.inverted_index(self.fields.namespace)?;
            let mut selected = 0u64;
            if let Some(mut postings) = namespaces.read_postings(
                &Term::from_field_text(self.fields.namespace, "memory"), IndexRecordOption::Basic)? {
                while postings.doc() != TERMINATED {
                    budget.charge(1)?;
                    let doc = postings.doc();
                    if members[doc as usize] == 1 { members[doc as usize] = 2; selected += 1; }
                    postings.advance();
                }
            }
            if selected == 0 { continue; }
            let generations = segment.inverted_index(self.fields.generation)?;
            let mut terms = generations.terms().stream()?;
            let mut covered = 0u64;
            while let Some((key, info)) = terms.next() {
                budget.charge(1)?;
                // Tantivy's numeric term dictionary uses serialized_value_bytes:
                // eight big-endian bytes, without the field or value-type tag.
                anyhow::ensure!(key.len() == 8,
                    "Source recovery found an unsupported lexical generation term");
                let generation = u64::from_be_bytes(key.try_into()?);
                let mut postings = generations.read_postings_from_terminfo(info, IndexRecordOption::Basic)?;
                while postings.doc() != TERMINATED {
                    budget.charge(1)?;
                    let member = &mut members[postings.doc() as usize];
                    if *member == 2 {
                        *member = 3;
                        covered += 1;
                        maximum = maximum.max(generation);
                    } else if *member == 3 {
                        anyhow::bail!("Source recovery found multiple physical generations for a lexical document");
                    }
                    postings.advance();
                }
            }
            anyhow::ensure!(covered == selected, "Source recovery found a lexical source document without a generation");
        }
        Ok(maximum)
    }

    #[cfg(feature = "vector")]
    async fn vector_source_high_water(&self, project: &str, budget: &mut WorkBudget<'_>) -> anyhow::Result<u64> {
        use arrow_array::{Array, UInt64Array};
        use futures::TryStreamExt;
        use lancedb::query::{QueryExecutionOptions, Select};
        let epoch = self.pin_vector_epoch(project)?;
        let path = self._lance_dir.join(format!("{}.lance", epoch.table));
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && epoch.record.is_none() => return Ok(0),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let remaining_time = budget.remaining_time()?;
        let allowed_rows = MAX_VECTOR_ROWS.min(budget.remaining as usize);
        let worker = self.clone();
        let result = self.run_native_vector_query(epoch.clone(), budget.cancel, async move {
            let table = worker.open_vector_epoch_table(&epoch).await?;
            let schema = table.schema().await?;
            anyhow::ensure!(schema.field_with_name("generation")?.data_type() == &arrow_schema::DataType::UInt64
                && !schema.field_with_name("generation")?.is_nullable()
                && schema.field_with_name("namespace")?.data_type() == &arrow_schema::DataType::Utf8
                && !schema.field_with_name("namespace")?.is_nullable(),
                "Source recovery found an unsupported vector generation schema");
            let mut options = QueryExecutionOptions::default();
            options.max_batch_length = 1024;
            options.timeout = Some(remaining_time);
            let query = table.query().only_if("namespace = 'memory'")
                .select(Select::Columns(vec!["generation".into()])).limit(allowed_rows + 1);
            let mut stream = query.execute_with_options(options).await?;
            let mut rows = 0usize;
            let mut maximum = 0;
            loop {
                let batch = stream.try_next().await?;
                let Some(batch) = batch else { break; };
                rows = rows.checked_add(batch.num_rows()).ok_or_else(|| anyhow::anyhow!("Vector row count overflow"))?;
                anyhow::ensure!(rows <= allowed_rows,
                    "Source recovery vector inspection work budget exceeded; source unchanged");
                let generations = batch.column_by_name("generation")
                    .and_then(|column| column.as_any().downcast_ref::<UInt64Array>())
                    .ok_or_else(|| anyhow::anyhow!("Source recovery vector generation column is missing or unsupported"))?;
                anyhow::ensure!(generations.null_count() == 0, "Source recovery found a null vector generation");
                for generation in generations.values() { maximum = maximum.max(*generation); }
            }
            Ok((maximum, rows))
        });
        // This bounds the caller's wait only. The owned native worker keeps its
        // epoch and slot if this deadline or request cancellation wins.
        let (maximum, rows) = tokio::time::timeout(remaining_time, result).await
            .map_err(|_| anyhow::anyhow!("Source recovery vector observation timed out; backend retains its reader lease"))??;
        budget.charge(rows as u64)?;
        Ok(maximum)
    }

    #[cfg(not(feature = "vector"))]
    async fn vector_source_high_water(&self, _project: &str, budget: &mut WorkBudget<'_>) -> anyhow::Result<u64> {
        // A no-vector build cannot certify existing native data as absent.
        for path in [&self._lance_dir, &self._tantivy_dir.join("source-snapshots/vector-routes")] {
            budget.check()?;
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
                Ok(metadata) => anyhow::ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Source recovery found an unsupported vector directory"),
            }
            anyhow::ensure!(std::fs::read_dir(path)?.next().transpose()?.is_none(),
                "Source recovery requires a vector-enabled build to inspect existing native vector metadata/data");
        }
        Ok(0)
    }

    /// Reserve a fresh, parentless snapshot without touching old physical rows.
    /// The caller's project update lock spans highwater, durable registry
    /// reservation, this preparation, indexing, and final registry publication.
    pub async fn prepare_recovered_source_snapshot(
        &self, project: &str, floor: u64, generation: u64, paths: &[RelPath], cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(generation > floor, "Recovered source generation must exceed its observed floor");
        let mutation = self.begin_project_mutation(project)?;
        let scoped = self.with_mutation(&mutation)?;
        let budget = WorkBudget::new(cancel);
        let _commit = tokio::time::timeout(budget.remaining_time()?, scoped.acquire_commit(cancel)).await
            .map_err(|_| anyhow::anyhow!("Recovered source preparation timed out waiting for storage work"))??;
        budget.check()?;
        anyhow::ensure!(paths.len() <= 250_000, "Recovered source snapshot exceeds its path entry budget");
        let mut path_bytes = 0u64;
        for path in paths {
            budget.check()?;
            path_bytes = path_bytes.saturating_add((path.as_str().len() as u64).saturating_mul(4).saturating_add(256));
        }
        anyhow::ensure!(path_bytes <= 64 * 1024 * 1024, "Recovered source snapshot exceeds its 64 MiB path budget");
        let _allocation = self.memory_budget.as_ref().map(|memory| AllocationGuard::try_new(
            memory, path_bytes, Subsystem::Misc, "recovered source snapshot path preparation")).transpose()?;
        scoped.snapshots.prepare_fresh_full(project, "memory", floor, generation,
            &paths.iter().map(|path| path.as_str().to_owned()).collect::<Vec<_>>(), &budget)
    }
}
